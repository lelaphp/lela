//! An entry read as php reads it: `zip_source_zip_file_create`'s stack
//! (`zip_source_zip_new.c`) of a window on the archive's file read by
//! position (`zip_source_window.c`), PKWARE's or `WinZip`'s decryption
//! (`zip_source_pkware_decode.c`, `zip_source_winzip_aes_decode.c`), the
//! method's decompressor over a buffer of `BUFSIZE` (`zip_source_compress.c`)
//! and the CRC checked at the end (`zip_source_crc.c`), behind `zip_fread`
//! and its loop (`zip_fread.c`, `zip_source_read.c`). Nothing here borrows
//! the archive, so a stream keeps one across PHP calls and holds a buffer,
//! not the entry; an error is met at the read that reaches it.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::os::unix::fs::FileExt;

use crate::codec::{PkwareKeys, WinzipAes};
use crate::error::{Er, Status};

/// `BUFSIZE`: what a decompressor reads from the layer below at once.
const BUFSIZE: usize = 8192;

/// The most one `zip_fread` hands its layers at once; php's buffer is
/// the whole length asked for, and a longer one is read in turns.
const ONE_READ: usize = 1 << 24;

/// A libzip error carried up through a codec's `io::Error`.
#[derive(Debug)]
struct Carried(Status);

impl std::fmt::Display for Carried {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "libzip error {}", self.0.zip)
    }
}

impl std::error::Error for Carried {}

fn carry(status: Status) -> io::Error {
    io::Error::other(Carried(status))
}

/// `_zip_source_window_new` over a file: `len` bytes from `start`, read
/// by position, so every window on one archive shares its file.
pub(crate) struct Window {
    file: File,
    start: u64,
    len: u64,
    offset: u64,
    /// The archive's own data, where libzip reads every failure as
    /// `ZIP_ER_EOF`; a file source's otherwise.
    archive: bool,
}

impl Window {
    pub(crate) fn new(file: File, start: u64, len: u64, archive: bool) -> Window {
        Window {
            file,
            start,
            len,
            offset: 0,
            archive,
        }
    }

    /// `window_read`: up to `buf.len()` bytes, `0` at the window's end.
    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, Status> {
        let left = usize::try_from(self.len - self.offset).unwrap_or(usize::MAX);
        let n = left.min(buf.len());
        if n == 0 {
            return Ok(0);
        }
        loop {
            return match self.file.read_at(&mut buf[..n], self.start + self.offset) {
                Ok(0) if self.archive => Err(Status::of(Er::Eof, 0)),
                Ok(0) => Err(Status::of(Er::DataLength, 0)),
                Ok(k) => {
                    self.offset += k as u64;
                    Ok(k)
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) if self.archive => Err(Status::of(Er::Eof, 0)),
                Err(e) => Err(Status::of(Er::Read, e.raw_os_error().unwrap_or(0))),
            };
        }
    }

    /// `n` bytes or `ZIP_ER_EOF`.
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Status> {
        let mut got = 0;
        while got < buf.len() {
            match self.read_some(&mut buf[got..])? {
                0 => return Err(Status::of(Er::Eof, 0)),
                n => got += n,
            }
        }
        Ok(())
    }
}

impl Read for Window {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_some(buf).map_err(carry)
    }
}

/// `zip_source_winzip_aes_decode`'s layer: the data between the header
/// and the authentication code, which the read past the data checks.
struct Aes {
    window: Window,
    ctx: Option<WinzipAes>,
    left: u64,
}

impl Aes {
    /// `winzip_aes_decrypt` on `ZIP_SOURCE_READ`.
    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, Status> {
        let n = usize::try_from(self.left)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        if buf.is_empty() {
            return Ok(0);
        }
        if n == 0 {
            let Some(ctx) = self.ctx.take() else {
                return Ok(0);
            };
            let mut code = [0u8; 10];
            self.window.read_exact(&mut code)?;
            if ctx.finish()[..10] != code {
                return Err(Status::of(Er::Crc, 0));
            }
            return Ok(0);
        }
        let k = self.window.read_some(&mut buf[..n])?;
        self.left -= k as u64;
        if let Some(ctx) = &mut self.ctx {
            ctx.decrypt(&mut buf[..k]);
        }
        Ok(k)
    }
}

/// The entry's stored bytes, decrypted where they are encrypted.
enum Raw {
    Plain(Window),
    Pkware(Window, PkwareKeys),
    Aes(Box<Aes>),
}

impl Raw {
    /// `zip_source_pkware_decode` opened: the twelve-byte header
    /// decrypted, its last byte the high byte of the DOS time or of the
    /// CRC where the entry has a CRC to check it by.
    fn pkware(
        mut window: Window,
        password: &[u8],
        check: Option<(u16, u32)>,
    ) -> Result<Raw, Status> {
        let mut keys = PkwareKeys::of(password);
        let mut header = [0u8; 12];
        window.read_exact(&mut header)?;
        keys.decrypt(&mut header);
        if let Some((time, crc)) = check
            && u16::from(header[11]) != time >> 8
            && u32::from(header[11]) != crc >> 24
        {
            return Err(Status::of(Er::Wrongpasswd, 0));
        }
        Ok(Raw::Pkware(window, keys))
    }

    /// `zip_source_winzip_aes_decode` opened: the salt read, the keys
    /// derived from it, the password verified.
    ///
    /// A wrong password fails the open; a damaged authentication code
    /// fails the read past the data, as a CRC error.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('enc-witness.zip', ZipArchive::CREATE|ZipArchive::OVERWRITE);
    /// foreach (['p' => ZipArchive::EM_TRAD_PKWARE, 'a' => ZipArchive::EM_AES_256] as $n => $m) {
    ///   $z->addFromString($n, str_repeat('0123456789abcdef', 512)); $z->setCompressionName($n, ZipArchive::CM_STORE); $z->setEncryptionName($n, $m, 'pw');
    /// }
    /// $z->close(); $z->open('enc-witness.zip');
    /// var_dump($z->getStream('a'), $z->getStatusString());
    /// $z->setPassword('no'); var_dump($z->getStream('p'), $z->getStatusString(), $z->getFromName('a'), $z->getStatusString());
    /// $z->setPassword('pw'); var_dump(strlen(stream_get_contents($z->getStream('a'))), strlen($z->getFromName('p', 5)));
    /// $z->close();
    /// $b = file_get_contents('enc-witness.zip'); $p = strpos($b, "PK\x01\x02") - 1; $b[$p] = chr(ord($b[$p]) ^ 1);
    /// file_put_contents('enc-witness.zip', $b); $z->open('enc-witness.zip'); $z->setPassword('pw');
    /// $f = $z->getStream('a'); var_dump(strlen(fread($f, 8192)), fread($f, 8192));
    /// $z->close(); unlink('enc-witness.zip');
    /// ```
    /// ```expect
    /// bool(false)
    /// string(20) "No password provided"
    /// bool(false)
    /// string(23) "Wrong password provided"
    /// bool(false)
    /// string(23) "Wrong password provided"
    /// int(8192)
    /// int(5)
    ///
    /// Warning: fread(): Zip stream error: CRC error in %s on line %d
    /// int(8192)
    /// bool(false)
    /// ```
    fn aes(
        mut window: Window,
        password: &[u8],
        strength: u8,
        comp_size: u64,
    ) -> Result<Raw, Status> {
        let salt_len = 4 * usize::from(strength) + 4;
        let aux = salt_len as u64 + 2 + 10;
        if comp_size < aux {
            return Err(Status::of(Er::Opnotsupp, 0));
        }
        let mut header = vec![0u8; salt_len + 2];
        window.read_exact(&mut header)?;
        let (ctx, verifier) = WinzipAes::new(password, &header[..salt_len], strength)?;
        if verifier != header[salt_len..] {
            return Err(Status::of(Er::Wrongpasswd, 0));
        }
        Ok(Raw::Aes(Box::new(Aes {
            window,
            ctx: Some(ctx),
            left: comp_size - aux,
        })))
    }
}

impl Read for Raw {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Raw::Plain(w) => w.read(buf),
            Raw::Pkware(w, keys) => {
                let n = w.read(buf)?;
                keys.decrypt(&mut buf[..n]);
                Ok(n)
            }
            Raw::Aes(a) => a.read_some(buf).map_err(carry),
        }
    }
}

/// One step of a `zip_compression_algorithm_t`'s `process`.
enum Step {
    Ok,
    End,
    NeedData,
    Error(Status),
}

/// How far a stream's running total moved.
fn moved(before: u64, after: u64) -> usize {
    usize::try_from(after - before).unwrap_or(usize::MAX)
}

/// `zip_algorithm_deflate.c` and `zip_algorithm_bzip2.c`, decompressing,
/// over the zlib the crate links and libbzip2 rewritten in Rust.
enum Algorithm {
    Deflate(flate2::Decompress),
    Bzip2(bzip2::Decompress),
}

impl Algorithm {
    /// `process`: what `input` makes of `out`, how much of each it took.
    fn process(
        &mut self,
        input: &[u8],
        out: &mut [u8],
        end_of_input: bool,
    ) -> (Step, usize, usize) {
        match self {
            Algorithm::Deflate(z) => {
                let (i, o) = (z.total_in(), z.total_out());
                let ret = z.decompress(input, out, flate2::FlushDecompress::Sync);
                let used = moved(i, z.total_in());
                let made = moved(o, z.total_out());
                let step = match ret {
                    Ok(flate2::Status::Ok) => Step::Ok,
                    Ok(flate2::Status::StreamEnd) => Step::End,
                    Ok(flate2::Status::BufError) if used == input.len() => Step::NeedData,
                    Ok(flate2::Status::BufError) => Step::Error(Status::of(Er::Zlib, -5)),
                    Err(e) if e.needs_dictionary().is_some() => {
                        Step::Error(Status::of(Er::Zlib, 2))
                    }
                    Err(_) => Step::Error(Status::of(Er::Zlib, -3)),
                };
                (step, used, made)
            }
            Algorithm::Bzip2(b) => {
                if input.is_empty() && !end_of_input {
                    return (Step::NeedData, 0, 0);
                }
                let (i, o) = (b.total_in(), b.total_out());
                let ret = b.decompress(input, out);
                let used = moved(i, b.total_in());
                let made = moved(o, b.total_out());
                let step = match ret {
                    Ok(bzip2::Status::StreamEnd) => Step::End,
                    Ok(bzip2::Status::MemNeeded) => Step::Error(Status::of(Er::Memory, 0)),
                    Ok(_) if used == input.len() => Step::NeedData,
                    Ok(_) => Step::Ok,
                    Err(bzip2::Error::Data | bzip2::Error::DataMagic) => {
                        Step::Error(Status::of(Er::CompressedData, 0))
                    }
                    Err(bzip2::Error::Param) => Step::Error(Status::of(Er::Inval, 0)),
                    Err(bzip2::Error::Sequence) => Step::Error(Status::of(Er::Internal, 0)),
                };
                (step, used, made)
            }
        }
    }
}

/// `zip_source_decompress`'s layer: `compress_read` feeding the
/// algorithm `BUFSIZE` bytes of the layer below at a time. What a step
/// that fails made is dropped, as libzip drops it.
#[expect(
    clippy::struct_excessive_bools,
    reason = "`struct context`'s own flags, one each"
)]
struct Decompressor {
    lower: Raw,
    algorithm: Algorithm,
    buffer: Vec<u8>,
    /// The part of `buffer` the algorithm has not taken.
    pending: std::ops::Range<usize>,
    end_of_input: bool,
    end_of_stream: bool,
    /// `first_read`: whether any input was read.
    started: bool,
    /// `ZIP_CHECKCONS`: input left past the stream's end is an error.
    check: bool,
    error: Option<Status>,
}

impl Decompressor {
    fn new(lower: Raw, algorithm: Algorithm, check: bool) -> Decompressor {
        Decompressor {
            lower,
            algorithm,
            buffer: vec![0; BUFSIZE],
            pending: 0..0,
            end_of_input: false,
            end_of_stream: false,
            started: false,
            check,
            error: None,
        }
    }

    /// `zip_source_read` on the layer below.
    fn read_lower(&mut self, len: usize) -> Result<usize, Status> {
        loop {
            return match self.lower.read(&mut self.buffer[..len]) {
                Ok(n) => Ok(n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => Err(e
                    .get_ref()
                    .and_then(|i| i.downcast_ref::<Carried>())
                    .map_or(Status::of(Er::Read, 0), |c| c.0)),
            };
        }
    }

    /// `compress_read`.
    ///
    /// A bzip2 entry damaged near its end: the stream reads up to the
    /// block whose check fails, and a read whose one step meets it gets
    /// nothing.
    ///
    /// ```php
    /// $d = ''; for ($i = 0; $i < 3000; $i++) { $d .= hash('sha256', "row $i") . "\n"; }
    /// $z = new ZipArchive; $z->open('bz-witness.zip', ZipArchive::CREATE|ZipArchive::OVERWRITE);
    /// $z->addFromString('b', $d); $z->setCompressionName('b', ZipArchive::CM_BZIP2); $z->close();
    /// $b = file_get_contents('bz-witness.zip'); $p = 31 + unpack('v', substr($b, 28, 2))[1] + unpack('V', substr($b, 18, 4))[1] - 40;
    /// $b[$p] = chr(ord($b[$p]) ^ 0x5a); file_put_contents('bz-witness.zip', $b);
    /// $z->open('bz-witness.zip'); $f = $z->getStream('b'); $n = 0;
    /// while (($r = fread($f, 8192)) !== false && $r !== '') { $n += strlen($r); } var_dump($n);
    /// var_dump(strlen($z->getFromName('b', 100)), strlen($z->getFromName('b')), $z->getFromName('b', 400000));
    /// $z->close(); unlink('bz-witness.zip');
    /// ```
    /// ```expect
    ///
    /// Warning: fread(): Zip stream error: Compressed data invalid in %s on line %d
    /// int(188416)
    /// int(100)
    /// int(195000)
    /// string(0) ""
    /// ```
    fn read_some(&mut self, out: &mut [u8]) -> Result<usize, Status> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if out.is_empty() || self.end_of_stream {
            return Ok(0);
        }
        let mut done = 0;
        let mut end = false;
        while !end && done < out.len() {
            let input = &self.buffer[self.pending.clone()];
            let (step, used, made) =
                self.algorithm
                    .process(input, &mut out[done..], self.end_of_input);
            self.pending.start += used;
            if !matches!(step, Step::Error(_)) {
                done += made;
            }
            match step {
                Step::End => {
                    self.end_of_stream = true;
                    if !self.end_of_input {
                        let left = match self.read_lower(1) {
                            Err(e) => {
                                self.error = Some(e);
                                break;
                            }
                            Ok(0) => {
                                self.end_of_input = true;
                                !self.pending.is_empty()
                            }
                            Ok(_) => true,
                        };
                        if left && self.check {
                            self.error = Some(Status::of(Er::Incons, 0));
                            break;
                        }
                    }
                    if !self.started {
                        self.error = Some(Status::of(Er::Internal, 0));
                        break;
                    }
                    end = true;
                }
                Step::Ok => {}
                Step::NeedData => {
                    if self.end_of_input {
                        end = true;
                        continue;
                    }
                    match self.read_lower(BUFSIZE) {
                        Err(e) => {
                            self.error = Some(e);
                            end = true;
                        }
                        Ok(0) => {
                            self.end_of_input = true;
                            self.started = true;
                            self.pending = 0..0;
                        }
                        Ok(n) => {
                            self.started = true;
                            self.pending = 0..n;
                        }
                    }
                }
                Step::Error(e) => {
                    self.error = Some(e);
                    end = true;
                }
            }
        }
        if done > 0 {
            return Ok(done);
        }
        self.error.map_or(Ok(0), Err)
    }
}

/// LZMA (method 14), whose header `zip_algorithm_xz.c` reads before the
/// first byte comes out: version, the properties' size, the properties.
struct Lzma {
    pending: Option<(BufReader<Raw>, u64)>,
    reader: Option<Box<lzma_rust2::LzmaReader<BufReader<Raw>>>>,
}

impl Read for Lzma {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some((mut src, size)) = self.pending.take() {
            let mut header = [0u8; 9];
            src.read_exact(&mut header)?;
            if header[2..4] != [5, 0] {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            let dict_size = u32::from_le_bytes([header[5], header[6], header[7], header[8]]);
            let reader =
                lzma_rust2::LzmaReader::new_with_props(src, size, header[4], dict_size, None)
                    .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            self.reader = Some(Box::new(reader));
        }
        match &mut self.reader {
            Some(r) => r.read(buf),
            None => Ok(0),
        }
    }
}

/// ZSTD (method 93), whose frame header ruzstd reads at its start.
struct Zstd {
    pending: Option<BufReader<Raw>>,
    reader: Option<
        Box<ruzstd::decoding::StreamingDecoder<BufReader<Raw>, ruzstd::decoding::FrameDecoder>>,
    >,
}

impl Read for Zstd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(src) = self.pending.take() {
            let reader = ruzstd::decoding::StreamingDecoder::new(src)
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            self.reader = Some(Box::new(reader));
        }
        match &mut self.reader {
            Some(r) => r.read(buf),
            None => Ok(0),
        }
    }
}

/// What a `zip_file_t` reads from below its CRC layer.
enum Body {
    /// Data held in memory: a string source, or none at all.
    Bytes { data: Vec<u8>, offset: usize },
    /// A window read as it is: stored data, or the raw bytes
    /// `ZIP_FL_COMPRESSED` asks for.
    Window(Window),
    /// Deflated or bzip2 data, decompressed as libzip decompresses it.
    Compressed(Box<Decompressor>),
    /// Decrypted, or decompressed by a codec read whole, and the error
    /// the codec's own failure reads as.
    Decoded {
        reader: Box<dyn Read + Send>,
        fault: Status,
    },
}

impl Body {
    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, Status> {
        match self {
            Body::Bytes { data, offset } => {
                let rest = data.get(*offset..).unwrap_or_default();
                let n = rest.len().min(buf.len());
                buf[..n].copy_from_slice(&rest[..n]);
                *offset += n;
                Ok(n)
            }
            Body::Window(w) => w.read_some(buf),
            Body::Compressed(d) => d.read_some(buf),
            Body::Decoded { reader, fault } => loop {
                return match reader.read(buf) {
                    Ok(n) => Ok(n),
                    Err(e) => match e.get_ref().and_then(|i| i.downcast_ref::<Carried>()) {
                        Some(c) => Err(c.0),
                        None if e.kind() == io::ErrorKind::Interrupted => continue,
                        // `ZIP_COMPRESSION_NEED_DATA` past the input's
                        // end: the stream ends there, and the CRC says
                        // what is missing.
                        None if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
                        None => Err(*fault),
                    },
                };
            },
        }
    }

    /// The position and length of a seekable body.
    fn span(&self) -> Option<(u64, u64)> {
        match self {
            Body::Bytes { data, offset } => Some((*offset as u64, data.len() as u64)),
            Body::Window(w) => Some((w.offset, w.len)),
            Body::Compressed(_) | Body::Decoded { .. } => None,
        }
    }

    fn set_offset(&mut self, to: u64) {
        match self {
            Body::Bytes { offset, .. } => *offset = usize::try_from(to).unwrap_or(usize::MAX),
            Body::Window(w) => w.offset = to,
            Body::Compressed(_) | Body::Decoded { .. } => {}
        }
    }
}

/// `struct crc_context` with `validate` set.
struct Crc {
    expect: u32,
    size: u64,
    hasher: crc32fast::Hasher,
    position: u64,
    /// `crc_position`: how far the CRC has read.
    hashed: u64,
    complete: bool,
}

/// `zip_file_t`: one entry opened for reading, its layers, and the
/// errors `zip_fread` and `zip_source_read` keep.
pub(crate) struct Reader {
    body: Body,
    crc: Option<Crc>,
    seekable: bool,
    /// `src->eof`.
    eof: bool,
    /// `src->had_read_error`, and the error it had.
    source_error: Option<Status>,
    /// `zf->error`.
    error: Option<Status>,
}

/// How a [`Reader`] is laid over an unchanged entry's data.
pub(crate) struct Layers<'a> {
    pub(crate) method: u16,
    pub(crate) encryption: u16,
    pub(crate) password: Option<&'a [u8]>,
    pub(crate) size: u64,
    pub(crate) comp_size: u64,
    /// The CRC to check the data against, where the stat has one.
    pub(crate) crc: Option<u32>,
    /// The DOS time PKWARE's header is checked by.
    pub(crate) dostime: u16,
    /// `ZIP_CHECKCONS`, which the archive was opened with.
    pub(crate) check: bool,
}

impl Reader {
    fn new(body: Body, crc: Option<(u32, u64)>, seekable: bool) -> Reader {
        Reader {
            body,
            crc: crc.map(|(expect, size)| Crc {
                expect,
                size,
                hasher: crc32fast::Hasher::new(),
                position: 0,
                hashed: 0,
                complete: false,
            }),
            seekable,
            eof: false,
            source_error: None,
            error: None,
        }
    }

    /// A string source's data.
    pub(crate) fn bytes(data: Vec<u8>) -> Reader {
        Reader::new(Body::Bytes { data, offset: 0 }, None, true)
    }

    /// A file whose archive was closed under it: every read fails with
    /// `ZIP_ER_ZIPCLOSED`.
    pub(crate) fn closed() -> Reader {
        let mut zf = Reader::bytes(Vec::new());
        zf.source_error = Some(Status::of(Er::Zipclosed, 0));
        zf
    }

    /// A window read as it is, its CRC checked where `crc` gives one.
    pub(crate) fn window(window: Window, crc: Option<(u32, u64)>) -> Reader {
        Reader::new(Body::Window(window), crc, true)
    }

    /// `zip_source_zip_file_create` past the stat, for an unchanged
    /// entry read whole: decryption, then decompression, then the CRC,
    /// each opened as libzip opens it.
    pub(crate) fn decoded(window: Window, l: &Layers<'_>) -> Result<Reader, Status> {
        let decrypt = l.password.is_some();
        if decrypt
            && !matches!(
                l.encryption,
                crate::archive::EM_TRAD_PKWARE
                    | crate::archive::EM_AES_128
                    | crate::archive::EM_AES_192
                    | crate::archive::EM_AES_256
            )
        {
            return Err(Status::of(Er::Encrnotsupp, 0));
        }
        if !matches!(l.method, 0 | 8 | 12 | 14 | 93 | 95) {
            return Err(Status::of(Er::Compnotsupp, 0));
        }
        let crc = l.crc.map(|c| (c, l.size));
        let empty = if decrypt || l.method != 0 {
            l.comp_size == 0
        } else {
            l.size == 0
        };
        if empty {
            return Ok(Reader::new(
                Body::Bytes {
                    data: Vec::new(),
                    offset: 0,
                },
                crc,
                false,
            ));
        }
        let raw = match l.password {
            None => Raw::Plain(window),
            Some(p) if l.encryption == crate::archive::EM_TRAD_PKWARE => {
                Raw::pkware(window, p, l.crc.map(|c| (l.dostime, c)))?
            }
            Some(p) => Raw::aes(window, p, (l.encryption & 0xff) as u8, l.comp_size)?,
        };
        if l.method == 0 {
            return Ok(match raw {
                Raw::Plain(w) => Reader::new(Body::Window(w), crc, true),
                raw => Reader::new(
                    Body::Decoded {
                        reader: Box::new(raw),
                        fault: Status::of(Er::Internal, 0),
                    },
                    crc,
                    false,
                ),
            });
        }
        let algorithm = match l.method {
            8 => Some(Algorithm::Deflate(flate2::Decompress::new(false))),
            12 => Some(Algorithm::Bzip2(bzip2::Decompress::new(false))),
            _ => None,
        };
        if let Some(algorithm) = algorithm {
            let d = Decompressor::new(raw, algorithm, l.check);
            return Ok(Reader::new(Body::Compressed(Box::new(d)), crc, false));
        }
        let src = BufReader::with_capacity(BUFSIZE, raw);
        let corrupt = Status::of(Er::CompressedData, 0);
        let (reader, fault): (Box<dyn Read + Send>, Status) = match l.method {
            14 => (
                Box::new(Lzma {
                    pending: Some((src, l.size)),
                    reader: None,
                }),
                corrupt,
            ),
            93 => (
                Box::new(Zstd {
                    pending: Some(src),
                    reader: None,
                }),
                corrupt,
            ),
            _ => (Box::new(lzma_rust2::XzReader::new(src, false)), corrupt),
        };
        Ok(Reader::new(Body::Decoded { reader, fault }, crc, false))
    }

    /// `zip_file_is_seekable`.
    pub(crate) fn seekable(&self) -> bool {
        self.seekable
    }

    /// `crc_read` on `ZIP_SOURCE_READ`: the body's bytes, the CRC and the
    /// length held to the stat's at their end.
    fn read_layer(&mut self, buf: &mut [u8]) -> Result<usize, Status> {
        let n = self.body.read_some(buf)?;
        let Some(c) = &mut self.crc else {
            return Ok(n);
        };
        if n == 0 {
            if c.hashed == c.position {
                c.complete = true;
                if c.hasher.clone().finalize() != c.expect {
                    return Err(Status::of(Er::Crc, 0));
                }
                if c.position != c.size {
                    return Err(Status::of(Er::Incons, 0));
                }
            }
        } else if !c.complete && c.position <= c.hashed {
            let skip = usize::try_from(c.hashed - c.position).unwrap_or(usize::MAX);
            if let Some(fresh) = buf[..n].get(skip..) {
                c.hasher.update(fresh);
                c.hashed += fresh.len() as u64;
            }
        }
        c.position += n as u64;
        Ok(n)
    }

    /// `zip_source_read`'s loop over `buf`: how much it filled, and the
    /// error that stopped it short.
    fn fill(&mut self, buf: &mut [u8]) -> (usize, Option<Status>) {
        let mut got = 0;
        while got < buf.len() {
            match self.read_layer(&mut buf[got..]) {
                Err(status) => return (got, Some(status)),
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => got += n,
            }
        }
        (got, None)
    }

    /// `zip_fread`: up to `len` bytes, fewer only at the end of the data
    /// or at an error, which the next read then fails with.
    pub(crate) fn read(&mut self, len: usize) -> Result<Vec<u8>, Status> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if len == 0 {
            return Ok(Vec::new());
        }
        if let Some(e) = self.source_error {
            self.error = Some(e);
            return Err(e);
        }
        let mut out = Vec::new();
        while out.len() < len && !self.eof {
            let start = out.len();
            let want = (len - start).min(ONE_READ);
            out.resize(start + want, 0);
            let (n, failed) = self.fill(&mut out[start..]);
            out.truncate(start + n);
            if let Some(status) = failed {
                self.source_error = Some(status);
                if out.is_empty() {
                    self.error = Some(status);
                    return Err(status);
                }
                break;
            }
            if n < want {
                break;
            }
        }
        Ok(out)
    }

    /// `zip_fseek`: a failed seek is the file's error from then on.
    pub(crate) fn seek(&mut self, offset: i64, whence: i32) -> bool {
        if self.error.is_some() {
            return false;
        }
        let target = self
            .body
            .span()
            .filter(|_| self.seekable)
            .and_then(|(at, len)| {
                let base = match whence {
                    1 => i64::try_from(at).ok()?,
                    2 => i64::try_from(len).ok()?,
                    _ => 0,
                };
                base.checked_add(offset)
                    .and_then(|t| u64::try_from(t).ok())
                    .filter(|t| *t <= len)
            });
        let Some(to) = target else {
            self.error = Some(Status::of(Er::Inval, 0));
            return false;
        };
        self.body.set_offset(to);
        if let Some(c) = &mut self.crc {
            c.position = to;
        }
        self.eof = false;
        true
    }

    /// `zip_ftell`: `-1` once the file has an error.
    pub(crate) fn tell(&self) -> i64 {
        if self.error.is_some() {
            return -1;
        }
        self.body
            .span()
            .map_or(0, |(at, _)| i64::try_from(at).unwrap_or(i64::MAX))
    }

    /// `zip_fclose`'s answer: whether no read failed.
    pub(crate) fn ok(&self) -> bool {
        self.error.is_none()
    }

    /// Every byte to the end, and the error that stopped it short.
    pub(crate) fn read_to_end(&mut self) -> (Vec<u8>, Option<Status>) {
        let mut out = Vec::new();
        loop {
            match self.read(ONE_READ) {
                Ok(chunk) if chunk.is_empty() => return (out, None),
                Ok(chunk) => out.extend_from_slice(&chunk),
                Err(status) => return (out, Some(status)),
            }
        }
    }
}
