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
use std::io::{self, Read};
use std::os::unix::fs::FileExt;

use crate::archive::Encryption;
use crate::codec::{PkwareKeys, WinzipAes};
use crate::error::{Detail, Er, Status};

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
        write!(f, "libzip error {}", self.0.er.code())
    }
}

impl std::error::Error for Carried {}

fn carry(status: Status) -> io::Error {
    io::Error::other(Carried(status))
}

/// The stdio `FILE` libzip reads an archive through
/// (`zip_source_file_stdio.c`), as the system's stdio buffers it: one
/// buffer of the file's `st_blksize`, refilled a block at a time, a read
/// longer than the buffer read straight past it with its last part kept
/// (`__fread`), and a seek inside what the buffer holds served from it
/// (`_fseeko`), so what it read stays readable after the file changes
/// under it.
pub(crate) struct Stdio {
    file: File,
    size: usize,
    /// What the buffer holds, the bytes before the file's offset.
    buffer: Vec<u8>,
    /// How much of it has been read.
    at: usize,
    /// The file's offset.
    offset: u64,
}

impl Stdio {
    pub(crate) fn new(file: File) -> Stdio {
        use std::os::unix::fs::MetadataExt;
        let size = file
            .metadata()
            .map_or(1024, |m| m.blksize())
            .clamp(1, 1 << 24);
        Stdio {
            file,
            size: usize::try_from(size).unwrap_or(1024),
            buffer: Vec::new(),
            at: 0,
            offset: 0,
        }
    }

    /// `read(2)` at the file's offset.
    fn sys_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.file.read_at(buf, self.offset) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(n) => {
                    self.offset += n as u64;
                    return Ok(n);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// `__srefill`: the buffer read again from the file's offset; `false`
    /// at the end of the file.
    fn refill(&mut self) -> io::Result<bool> {
        let mut buffer = std::mem::take(&mut self.buffer);
        buffer.resize(self.size, 0);
        let got = self.sys_read(&mut buffer);
        buffer.truncate(*got.as_ref().unwrap_or(&0));
        self.buffer = buffer;
        self.at = 0;
        got.map(|n| n > 0)
    }

    /// `_fseeko` to `target`.
    pub(crate) fn seek(&mut self, target: u64) {
        let held = self.buffer.len() as u64;
        let first = self.offset - held;
        if target >= first && target < first + held {
            self.at = usize::try_from(target - first).unwrap_or(0);
            return;
        }
        let block = target & !(self.size as u64 - 1);
        self.offset = block;
        self.buffer.clear();
        self.at = 0;
        let into = usize::try_from(target - block).unwrap_or(0);
        if into > 0 {
            if self.refill().unwrap_or(false) && self.buffer.len() >= into {
                self.at = into;
            } else {
                self.offset = target;
                self.buffer.clear();
            }
        }
    }

    /// `__fread`: up to `out.len()` bytes, fewer only at the file's end.
    ///
    /// An entry read after its archive is cut reads what the buffer held:
    /// the block its local header was read from, less the header.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('stdio-witness.zip', ZipArchive::CREATE|ZipArchive::OVERWRITE);
    /// $z->addFromString('st', str_repeat('x', 195000)); $z->setCompressionName('st', ZipArchive::CM_STORE); $z->close();
    /// $z->open('stdio-witness.zip'); $s = $z->getStream('st'); $h = fopen('stdio-witness.zip', 'r+'); ftruncate($h, 1000); fclose($h);
    /// $t = 0; while (!feof($s)) { $r = fread($s, 8192); if ($r === false) break; $t += strlen($r); } echo $t, "\n";
    /// var_dump(strlen($z->getFromName('st'))); $z->close(); unlink('stdio-witness.zip');
    /// ```
    /// ```expect
    /// 4064
    ///
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Premature end of file in %s on line %d
    /// int(0)
    /// ```
    pub(crate) fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let held = (self.buffer.len() - self.at).min(out.len());
        out[..held].copy_from_slice(&self.buffer[self.at..self.at + held]);
        self.at += held;
        let mut got = held;
        let rest = out.len() - got;
        if rest > self.size {
            while got < out.len() {
                let n = self.sys_read(&mut out[got..])?;
                if n == 0 {
                    self.buffer.clear();
                    self.at = 0;
                    return Ok(got);
                }
                got += n;
            }
            let kept = rest - self.size * ((rest - 1) / self.size);
            self.buffer = out[out.len() - kept..].to_vec();
            self.at = kept;
            return Ok(got);
        }
        while got < out.len() {
            if !self.refill()? {
                return Ok(got);
            }
            let n = (out.len() - got).min(self.buffer.len());
            out[got..got + n].copy_from_slice(&self.buffer[..n]);
            self.at = n;
            got += n;
        }
        Ok(got)
    }
}

/// `_zip_source_window_new` over a file: `len` bytes from `start`, read
/// through a stdio `FILE`, its own or the archive's it is lent.
pub(crate) struct Window {
    file: Stdio,
    start: u64,
    len: u64,
    offset: u64,
    /// The archive's own data, where libzip reads every failure as
    /// `ZIP_ER_EOF` and reads through the archive's `FILE`; a file
    /// source's otherwise, read through its own.
    archive: bool,
}

impl Window {
    pub(crate) fn new(file: Stdio, start: u64, len: u64, archive: bool) -> Window {
        Window {
            file,
            start,
            len,
            offset: 0,
            archive,
        }
    }

    /// `window_read`: the seek to where the window is read to, then up to
    /// `buf.len()` bytes, `0` at the window's end.
    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, Status> {
        let left = usize::try_from(self.len - self.offset).unwrap_or(usize::MAX);
        let n = left.min(buf.len());
        if n == 0 {
            return Ok(0);
        }
        self.file.seek(self.start + self.offset);
        match self.file.read(&mut buf[..n]) {
            Ok(0) | Err(_) if self.archive => Err(Status::new(Er::Eof)),
            Ok(0) => Err(Status::new(Er::DataLength)),
            Ok(k) => {
                self.offset += k as u64;
                Ok(k)
            }
            Err(e) => Err(Status::os(Er::Read, &e)),
        }
    }

    /// `n` bytes or `ZIP_ER_EOF`.
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Status> {
        let mut got = 0;
        while got < buf.len() {
            match self.read_some(&mut buf[got..])? {
                0 => return Err(Status::new(Er::Eof)),
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
                return Err(Status::new(Er::Crc));
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
            return Err(Status::new(Er::Wrongpasswd));
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
    /// $z->setPassword('no'); var_dump($z->getFromName('a'), $z->getStatusString());
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
            return Err(Status::new(Er::Opnotsupp));
        }
        let mut header = vec![0u8; salt_len + 2];
        window.read_exact(&mut header)?;
        let (ctx, verifier) = WinzipAes::new(password, &header[..salt_len], strength)?;
        if verifier != header[salt_len..] {
            return Err(Status::new(Er::Wrongpasswd));
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

impl Raw {
    fn window(&mut self) -> &mut Window {
        match self {
            Raw::Plain(w) | Raw::Pkware(w, _) => w,
            Raw::Aes(a) => &mut a.window,
        }
    }
}

/// `zip_source_read` on the stored bytes: up to `buf.len()` of them.
fn read_raw(lower: &mut Raw, buf: &mut [u8]) -> Result<usize, Status> {
    loop {
        return match lower.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => Err(e
                .get_ref()
                .and_then(|i| i.downcast_ref::<Carried>())
                .map_or(Status::new(Er::Read), |c| c.0)),
        };
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
                    Ok(flate2::Status::BufError) => Step::Error(Status::zlib(-5)),
                    Err(e) if e.needs_dictionary().is_some() => Step::Error(Status::zlib(2)),
                    Err(_) => Step::Error(Status::zlib(-3)),
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
                    Ok(bzip2::Status::MemNeeded) => Step::Error(Status::new(Er::Memory)),
                    Ok(_) if used == input.len() => Step::NeedData,
                    Ok(_) => Step::Ok,
                    Err(bzip2::Error::Data | bzip2::Error::DataMagic) => {
                        Step::Error(Status::new(Er::CompressedData))
                    }
                    Err(bzip2::Error::Param) => Step::Error(Status::new(Er::Inval)),
                    Err(bzip2::Error::Sequence) => Step::Error(Status::new(Er::Internal)),
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
        read_raw(&mut self.lower, &mut self.buffer[..len])
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
    ///
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Compressed data invalid in %s on line %d
    /// int(100)
    /// int(195000)
    /// bool(false)
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
                            self.error = Some(Status::new(Er::Incons));
                            break;
                        }
                    }
                    if !self.started {
                        self.error = Some(Status::new(Er::Internal));
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

/// The stored bytes a codec its crate reads whole reads from, as
/// `compress_read` hands them to liblzma and libzstd: `BUFSIZE` at a
/// time, each read of the layer below numbered, so a byte the codec
/// makes is known by the reads it was made from.
struct Feed {
    lower: Raw,
    buffer: Vec<u8>,
    at: usize,
    /// The reads of the layer below so far.
    reads: usize,
    /// The read that met the data's end, and the error it failed with.
    end: Option<(usize, Option<Status>)>,
    /// Whether the codec asked past the end.
    starved: bool,
    /// The bytes of the zstd frame being read, which its blocks are read
    /// again from.
    kept: Option<Vec<u8>>,
}

impl Feed {
    fn new(lower: Raw) -> Feed {
        Feed {
            lower,
            buffer: Vec::new(),
            at: 0,
            reads: 0,
            end: None,
            starved: false,
            kept: None,
        }
    }

    fn left(&self) -> usize {
        self.buffer.len() - self.at
    }

    /// One more read of the layer below behind what is left; `false` at
    /// the data's end or an error.
    fn fetch(&mut self) -> bool {
        if self.end.is_some() {
            return false;
        }
        self.reads += 1;
        self.buffer.drain(..self.at);
        self.at = 0;
        let have = self.buffer.len();
        self.buffer.resize(have + BUFSIZE, 0);
        let got = read_raw(&mut self.lower, &mut self.buffer[have..]);
        self.buffer.truncate(have + got.unwrap_or(0));
        match got {
            Ok(0) => self.end = Some((self.reads, None)),
            Err(e) => self.end = Some((self.reads, Some(e))),
            Ok(_) => return true,
        }
        false
    }
}

impl Read for Feed {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.left() == 0 && !self.fetch() {
            self.starved = true;
            return Ok(0);
        }
        let n = self.left().min(out.len());
        out[..n].copy_from_slice(&self.buffer[self.at..self.at + n]);
        if let Some(kept) = &mut self.kept {
            kept.extend_from_slice(&out[..n]);
        }
        self.at += n;
        Ok(n)
    }
}

/// How a codec read whole stopped.
#[derive(Clone, Copy)]
enum Stop {
    /// `ZIP_COMPRESSION_END`.
    End,
    /// The library's error, as libzip's `map_error` reads its code.
    Failed(Status),
    /// It wanted input past the data's end: the stream ends there, and
    /// the CRC says what is missing.
    Starved,
}

/// The largest frame window libzstd decodes by default
/// (`ZSTD_WINDOWLOG_LIMIT_DEFAULT`).
const ZSTD_WINDOW_MAX: u64 = 1 << 27;

/// What libzstd made of a frame's first blocks: the frame read again
/// with an empty last block after them.
fn zstd_prefix(frame: &[u8]) -> Vec<u8> {
    let mut src = frame.to_vec();
    src.extend_from_slice(&[1, 0, 0]);
    if frame.get(4).is_some_and(|d| d & 4 != 0) {
        src.extend_from_slice(&[0; 4]);
    }
    let mut d = ruzstd::decoding::FrameDecoder::new();
    d.set_max_window_size(ZSTD_WINDOW_MAX);
    let mut s = &src[..];
    if d.init(&mut s).is_err()
        || d.decode_blocks(&mut s, ruzstd::decoding::BlockDecodingStrategy::All)
            .is_err()
    {
        return Vec::new();
    }
    d.collect().unwrap_or_default()
}

/// libzstd's frames, read one block at a time by ruzstd
/// (`ZSTD_decompressStream`).
struct Zstd {
    feed: Feed,
    frame: Option<ruzstd::decoding::FrameDecoder>,
    /// Where each block read of the frame ends in its kept bytes, and the
    /// read of the layer below that completed it.
    blocks: Vec<(usize, usize)>,
    /// The frame's bytes ruzstd has let go of.
    made: usize,
}

/// What one turn of a codec read whole made.
enum Made {
    Bytes(Vec<u8>, usize),
    /// A zstd frame's last bytes, behind which a new step begins.
    FrameEnd(Vec<u8>, usize),
    /// A zstd frame's bytes libzstd delivers before it stops: those
    /// made before the read the stop is met at, and those made at it.
    Before(Vec<u8>, Vec<u8>, Stop, usize),
    Stopped(Stop, usize),
}

impl Zstd {
    /// The frame's blocks read so far, as libzstd delivers them before
    /// it meets `stop` at read `at`: those read before `at`, then those
    /// read at it.
    fn before(&mut self, made: Option<Vec<u8>>, stop: Stop, at: usize) -> Made {
        let kept = self.feed.kept.take().unwrap_or_default();
        let k = self.blocks.len();
        let all = match made {
            Some(tail) => tail,
            None if k == 0 => Vec::new(),
            None => zstd_prefix(&kept[..self.blocks[k - 1].0])
                .get(self.made..)
                .unwrap_or_default()
                .to_vec(),
        };
        let first = self.blocks.iter().position(|b| b.1 >= at).unwrap_or(k);
        let early = match first {
            0 => 0,
            f if f == k => self.made + all.len(),
            f => zstd_prefix(&kept[..self.blocks[f - 1].0]).len(),
        };
        let split = early.saturating_sub(self.made).min(all.len());
        self.frame = None;
        if all.is_empty() {
            return Made::Stopped(stop, at);
        }
        let (a, b) = all.split_at(split);
        Made::Before(a.to_vec(), b.to_vec(), stop, at)
    }

    /// One frame header, block or skipped frame read.
    fn turn(&mut self) -> Made {
        use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
        let fault = Status::new(Er::Internal);
        let Some(d) = &mut self.frame else {
            self.feed.kept = Some(Vec::new());
            self.blocks.clear();
            self.made = 0;
            let mut d = ruzstd::decoding::FrameDecoder::new();
            d.set_max_window_size(ZSTD_WINDOW_MAX);
            return match d.init(&mut self.feed) {
                Ok(()) => {
                    self.frame = Some(d);
                    Made::Bytes(Vec::new(), self.feed.reads)
                }
                Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                    length,
                    ..
                })) => {
                    let mut skip = vec![0; length as usize];
                    self.feed.read_exact(&mut skip).ok();
                    if self.feed.starved {
                        Made::Stopped(Stop::Starved, self.feed.reads)
                    } else {
                        Made::Bytes(Vec::new(), self.feed.reads)
                    }
                }
                Err(_) if self.feed.starved => Made::Stopped(Stop::Starved, self.feed.reads),
                Err(_) => Made::Stopped(Stop::Failed(fault), self.feed.reads),
            };
        };
        let got = d.decode_blocks(
            &mut self.feed,
            ruzstd::decoding::BlockDecodingStrategy::UptoBlocks(1),
        );
        let at = self.feed.reads;
        match got {
            Err(_) if self.feed.starved => self.before(None, Stop::Starved, at),
            Err(_) => self.before(None, Stop::Failed(fault), at),
            Ok(finished) => {
                let end = self.feed.kept.as_ref().map_or(0, Vec::len);
                self.blocks.push((end, at));
                let bytes = d.collect().unwrap_or_default();
                if !finished {
                    self.made += bytes.len();
                    return Made::Bytes(bytes, at);
                }
                let sums = (d.get_checksum_from_data(), d.get_calculated_checksum());
                if let (Some(stored), Some(made)) = sums
                    && stored != made
                {
                    return self.before(Some(bytes), Stop::Failed(fault), at);
                }
                self.frame = None;
                Made::FrameEnd(bytes, at)
            }
        }
    }
}

/// A codec its crate reads whole.
enum Codec {
    /// LZMA before libzip's header of it (`zip_algorithm_xz.c`'s
    /// `input`): its feed, and the size it ends at where the entry has no
    /// end marker.
    LzmaHeader(Option<Feed>, u64),
    Lzma(Box<lzma_rust2::LzmaReader<Feed>>),
    /// XZ, and whether its first bytes are XZ's magic, liblzma's
    /// `LZMA_FORMAT_ERROR` otherwise.
    Xz(Box<lzma_rust2::XzReader<Feed>>, Option<bool>),
    Zstd(Box<Zstd>),
}

impl Codec {
    fn feed(&mut self) -> Option<&mut Feed> {
        match self {
            Codec::LzmaHeader(feed, _) => feed.as_mut(),
            Codec::Lzma(r) => Some(r.inner_mut()),
            Codec::Xz(r, _) => Some(r.inner_mut()),
            Codec::Zstd(z) => Some(&mut z.feed),
        }
    }

    /// libzip's header of an LZMA entry, then the reader over the rest.
    fn lzma(feed: Option<Feed>, size: u64) -> (Codec, Made) {
        let Some(mut feed) = feed else {
            return (
                Codec::LzmaHeader(None, size),
                Made::Stopped(Stop::Failed(Status::new(Er::Internal)), 0),
            );
        };
        while feed.left() < 14 && feed.fetch() {}
        let at = feed.reads;
        let head = &feed.buffer[feed.at..];
        let corrupt = Made::Stopped(Stop::Failed(Status::new(Er::CompressedData)), at);
        if head.len() >= 9 && head[2..4] != [5, 0] {
            return (Codec::LzmaHeader(Some(feed), size), corrupt);
        }
        if head.len() < 14 {
            return (
                Codec::LzmaHeader(Some(feed), size),
                Made::Stopped(Stop::Starved, at),
            );
        }
        let (props, dict) = (
            head[4],
            u32::from_le_bytes([head[5], head[6], head[7], head[8]]),
        );
        if props > 224 || props % 9 + props / 9 % 5 > 4 {
            // `lzma_lzma_lclppb_decode`'s `LZMA_FORMAT_ERROR` leaves the
            // stream in error, and the next `lzma_code` is liblzma's
            // `LZMA_PROG_ERROR`, which `map_error` reads as internal.
            return (
                Codec::LzmaHeader(Some(feed), size),
                Made::Stopped(Stop::Failed(Status::new(Er::Internal)), at),
            );
        }
        feed.at += 9;
        match lzma_rust2::LzmaReader::new_with_props(feed, size, props, dict, None) {
            Ok(r) => (Codec::Lzma(Box::new(r)), Made::Bytes(Vec::new(), at)),
            Err(_) => (Codec::LzmaHeader(None, size), corrupt),
        }
    }

    /// One read of the codec: at most `room` bytes, and for LZMA fewer
    /// than the input left could make, so none is made from a read the
    /// input past it had not come in by (a symbol takes at most 48 bytes).
    fn turn(&mut self, room: usize, scratch: &mut [u8]) -> Made {
        let (got, magic) = match self {
            Codec::LzmaHeader(feed, size) => {
                let (codec, made) = Codec::lzma(feed.take(), *size);
                *self = codec;
                return made;
            }
            Codec::Zstd(z) => return z.turn(),
            Codec::Lzma(r) => {
                let n = (r.inner().left().saturating_sub(64) / 48).clamp(1, room);
                (r.read(&mut scratch[..n]), true)
            }
            Codec::Xz(r, magic) => {
                let magic = *magic.get_or_insert_with(|| {
                    let feed = r.inner_mut();
                    while feed.left() < 6 && feed.fetch() {}
                    feed.buffer[feed.at..].starts_with(b"\xfd7zXZ\0")
                });
                // An LZMA2 chunk is read whole, so the reads its bytes
                // were made from are known no closer than the chunk.
                (r.read(&mut scratch[..room]), magic)
            }
        };
        let Some(feed) = self.feed() else {
            return Made::Stopped(Stop::Failed(Status::new(Er::Internal)), 0);
        };
        let at = feed.reads;
        match got {
            Ok(0) => Made::Stopped(Stop::End, at),
            _ if feed.starved => Made::Stopped(Stop::Starved, at),
            Ok(m) => Made::Bytes(scratch[..m].to_vec(), at),
            Err(_) => Made::Stopped(
                Stop::Failed(Status::new(if magic {
                    Er::CompressedData
                } else {
                    Er::Internal
                })),
                at,
            ),
        }
    }
}

/// LZMA, XZ and ZSTD as `compress_read` drives liblzma and libzstd
/// through `zip_algorithm_xz.c` and `zip_algorithm_zstd.c`, over codecs
/// their crates read whole: each byte is made ahead, marked with the
/// reads of the layer below it was made from, and handed on as libzip's
/// `process` steps would hand it, a step ending where the buffer it fills
/// is full or the input runs out; a step that meets the library's error
/// drops what it made.
struct Pulled {
    codec: Codec,
    ahead: std::collections::VecDeque<u8>,
    /// Runs of `ahead`: the reads they were made from, and their length.
    runs: std::collections::VecDeque<(usize, usize)>,
    /// The bytes at the head of `ahead` a zstd frame's end keeps out of
    /// the step after it.
    keep: usize,
    stop: Option<(Stop, usize)>,
    /// `compress_read`'s reads of the layer below.
    reads: usize,
    end_of_input: bool,
    end_of_stream: bool,
    error: Option<Status>,
    /// `ZIP_CHECKCONS`: input left past the stream's end is an error.
    check: bool,
    scratch: Vec<u8>,
}

impl Pulled {
    fn new(codec: Codec, check: bool) -> Pulled {
        Pulled {
            codec,
            ahead: std::collections::VecDeque::new(),
            runs: std::collections::VecDeque::new(),
            keep: 0,
            stop: None,
            reads: 0,
            end_of_input: false,
            end_of_stream: false,
            error: None,
            check,
            scratch: vec![0; BUFSIZE],
        }
    }

    fn push(&mut self, bytes: &[u8], at: usize) {
        if bytes.is_empty() {
            return;
        }
        self.ahead.extend(bytes);
        match self.runs.back_mut() {
            Some(run) if run.0 == at => run.1 += bytes.len(),
            _ => self.runs.push_back((at, bytes.len())),
        }
    }

    /// The bytes ahead made from the first `reads` reads.
    fn ready(&self) -> usize {
        self.runs
            .iter()
            .take_while(|r| r.0 <= self.reads)
            .map(|r| r.1)
            .sum()
    }

    fn take(&mut self, out: &mut [u8]) -> usize {
        let n = out.len();
        for (o, b) in out.iter_mut().zip(self.ahead.drain(..n)) {
            *o = b;
        }
        let mut left = n;
        while left > 0 {
            let Some(run) = self.runs.front_mut() else {
                break;
            };
            let k = run.1.min(left);
            run.1 -= k;
            left -= k;
            if run.1 == 0 {
                self.runs.pop_front();
            }
        }
        self.keep = self.keep.saturating_sub(n);
        n
    }

    /// The codec read on until `space` bytes are ready, it reads past
    /// what `compress_read` has read, or it stops.
    fn make(&mut self, space: usize) {
        while self.stop.is_none()
            && self.ready() <= space
            && self.codec.feed().is_some_and(|f| f.reads <= self.reads)
        {
            let room = space
                .saturating_sub(self.ahead.len() + 1)
                .clamp(1, self.scratch.len());
            match self.codec.turn(room, &mut self.scratch) {
                Made::Bytes(b, at) => self.push(&b, at),
                Made::FrameEnd(b, at) => {
                    self.push(&b, at);
                    self.keep = self.ahead.len();
                }
                Made::Before(early, late, stop, at) => {
                    self.push(&early, at);
                    self.keep = self.ahead.len();
                    self.push(&late, at);
                    self.stop = Some((stop, at));
                }
                Made::Stopped(stop, at) => self.stop = Some((stop, at)),
            }
        }
    }

    /// `compress_read`'s read of one byte past the stream's end: whether
    /// any input is left.
    fn trailing(&mut self) -> Result<bool, Status> {
        let reads = self.reads;
        let Some(feed) = self.codec.feed() else {
            return Ok(false);
        };
        let more = if feed.reads > reads {
            match feed.end {
                Some((at, Some(e))) if at == reads + 1 => return Err(e),
                Some((at, None)) if at == reads + 1 => false,
                _ => true,
            }
        } else {
            read_raw(&mut feed.lower, &mut [0u8; 1])? > 0
        };
        Ok(more || feed.left() > 0)
    }

    /// `compress_read`.
    ///
    /// LZMA, XZ and ZSTD entries the oracle wrote, damaged: a step that
    /// meets liblzma's error drops what it made; LZMA whose input ends before
    /// its end marker ends there, and the CRC finds it short; liblzma's
    /// `LZMA_FORMAT_ERROR` and every ZSTD error are libzip's `Internal error`
    /// (its `map_error` is handed `ZSTD_decompressStream`'s return, which no
    /// case matches); and libzstd reads on through damage to a frame without
    /// a checksum, which the CRC then finds. `getFromName` meets the same
    /// errors, as php 8.5.12 reads an entry through to its end.
    ///
    /// ```php
    /// $z = base64_decode('UEsDBD8AAgAOAJIQQ10RaoskYwAAAPBVAAABAAAAbAkUBQBdAAAEAAAwiAmG1B9FuCOP9NI7P2QgG4TEbe+6YngwQwLuv8NzyJHDoCyOV1cqVIHHUAW6YehSADuHGILHHs/JY6H+MavdjihXb3j28THGD/KTQG+yO57q7P//xtxAAFBLAwQUAAAAXwCSEENdEWqLJJQAAADwVQAAAQAAAHj9N3pYWgAABObWtEYCACEBDAAAAI+YQZzgVe8AU10AMIgJhtQfRbgjj/TSOz9kIBuExG3vumJ4MEMC7r/Dc8iRw6AsjldXKlSBx1AFumHoUgA7hxiCxx7PyWOh/jGr3Y4oV2949vExxg/yk0Bvsjn6GAAAANpQsC7u8a+SAAFv8KsBAAA8zWvpscRn+wIAAAAABFlaUEsDBBQAAABdAJIQQ10RaoskJwAAAPBVAAABAAAAeii1L/0AWPUAALBhIGxpbmUgb2YgdGhlIHdpdG5lc3MKAQCuq9xnE1BLAQI/Az8AAgAOAJIQQ10RaoskYwAAAPBVAAABAAAAAAAAAAAAAAC2gQAAAABsUEsBAj8DFAAAAF8AkhBDXRFqiySUAAAA8FUAAAEAAAAAAAAAAAAAALaBggAAAHhQSwECPwMUAAAAXQCSEENdEWqLJCcAAADwVQAAAQAAAAAAAAAAAAAAtoE1AQAAelBLBQYAAAAAAwADAI0AAAB7AQAAAAA=');
    /// foreach ([['l', 31, 4], ['l', 31, 58], ['l', 31, 68], ['x', 161, 5], ['x', 161, 97], ['z', 340, 3], ['z', 340, 15]] as [$n, $s, $o]) {
    ///   $b = $z; $b[$s + $o] = chr(ord($b[$s + $o]) ^ 0x5a); file_put_contents('codec-witness.zip', $b);
    ///   $a = new ZipArchive; $a->open('codec-witness.zip'); $f = $a->getStream($n); $k = 0;
    ///   while (($r = fread($f, 4096)) !== false && $r !== '') { $k += strlen($r); }
    ///   $g = [strlen($a->getFromName($n)), strlen($a->getFromName($n, 100))];
    ///   echo "$n+$o: $k ", implode(' ', $g), "\n"; $a->close();
    /// }
    /// unlink('codec-witness.zip');
    /// ```
    /// ```expect
    ///
    /// Warning: fread(): Zip stream error: Internal error in %s on line %d
    ///
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Internal error in %s on line %d
    ///
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Internal error in %s on line %d
    /// l+4: 0 0 0
    ///
    /// Warning: fread(): Zip stream error: CRC error in %s on line %d
    ///
    /// Warning: fread(): Zip stream error: CRC error in %s on line %d
    ///
    /// Warning: ZipArchive::getFromName(): Cannot read entry: CRC error in %s on line %d
    /// l+58: 5504 0 100
    ///
    /// Warning: fread(): Zip stream error: Compressed data invalid in %s on line %d
    ///
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Compressed data invalid in %s on line %d
    /// l+68: 8192 0 100
    ///
    /// Warning: fread(): Zip stream error: Internal error in %s on line %d
    ///
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Internal error in %s on line %d
    ///
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Internal error in %s on line %d
    /// x+5: 0 0 0
    ///
    /// Warning: fread(): Zip stream error: Compressed data invalid in %s on line %d
    ///
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Compressed data invalid in %s on line %d
    /// x+97: 8192 0 100
    ///
    /// Warning: fread(): Zip stream error: Internal error in %s on line %d
    ///
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Internal error in %s on line %d
    ///
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Internal error in %s on line %d
    /// z+3: 0 0 0
    ///
    /// Warning: fread(): Zip stream error: CRC error in %s on line %d
    ///
    /// Warning: fread(): Zip stream error: CRC error in %s on line %d
    ///
    /// Warning: ZipArchive::getFromName(): Cannot read entry: CRC error in %s on line %d
    /// z+15: 22000 0 100
    /// ```
    ///
    /// Four damages read otherwise here than under liblzma and libzstd,
    /// each a property of the pure-Rust codec ext/zip drives (revision 70)
    /// and a divergence row of its own, ruled on #1648; the fences pin the
    /// oracle's bytes, and the ZSTD row's two halves are a fence each.
    ///
    /// An XZ entry's LZMA2 chunk header with its compressed size damaged:
    /// liblzma reads the chunk's first 16384 bytes before its error, and
    /// lzma-rust2 reads a chunk whole before it makes a byte, so the CRC
    /// finds the entry empty.
    ///
    /// ```php
    /// $z = base64_decode('UEsDBD8AAgAOAJIQQ10RaoskYwAAAPBVAAABAAAAbAkUBQBdAAAEAAAwiAmG1B9FuCOP9NI7P2QgG4TEbe+6YngwQwLuv8NzyJHDoCyOV1cqVIHHUAW6YehSADuHGILHHs/JY6H+MavdjihXb3j28THGD/KTQG+yO57q7P//xtxAAFBLAwQUAAAAXwCSEENdEWqLJJQAAADwVQAAAQAAAHj9N3pYWgAABObWtEYCACEBDAAAAI+YQZzgVe8AU10AMIgJhtQfRbgjj/TSOz9kIBuExG3vumJ4MEMC7r/Dc8iRw6AsjldXKlSBx1AFumHoUgA7hxiCxx7PyWOh/jGr3Y4oV2949vExxg/yk0Bvsjn6GAAAANpQsC7u8a+SAAFv8KsBAAA8zWvpscRn+wIAAAAABFlaUEsDBBQAAABdAJIQQ10RaoskJwAAAPBVAAABAAAAeii1L/0AWPUAALBhIGxpbmUgb2YgdGhlIHdpdG5lc3MKAQCuq9xnE1BLAQI/Az8AAgAOAJIQQ10RaoskYwAAAPBVAAABAAAAAAAAAAAAAAC2gQAAAABsUEsBAj8DFAAAAF8AkhBDXRFqiySUAAAA8FUAAAEAAAAAAAAAAAAAALaBggAAAHhQSwECPwMUAAAAXQCSEENdEWqLJCcAAADwVQAAAQAAAAAAAAAAAAAAtoE1AQAAelBLBQYAAAAAAwADAI0AAAB7AQAAAAA=');
    /// $z[188] = chr(ord($z[188]) ^ 0x5a); file_put_contents('codec-xz-chunk.zip', $z);
    /// $a = new ZipArchive; $a->open('codec-xz-chunk.zip'); $f = $a->getStream('x'); $k = 0;
    /// while (($r = fread($f, 8192)) !== false && $r !== '') { $k += strlen($r); }
    /// echo "$k ", strlen($a->getFromName('x')), "\n"; $a->close();
    /// unlink('codec-xz-chunk.zip');
    /// ```
    /// ```expect
    ///
    /// Warning: fread(): Zip stream error: Compressed data invalid in %s on line %d
    /// 16384%s
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Compressed data invalid in %s on line %d
    /// 0
    /// ```
    ///
    /// An LZMA entry's last byte damaged, after its end marker: liblzma's
    /// `rc_is_finished` makes it `Compressed data invalid`, and lzma-rust2,
    /// which does not check the range coder's end, reads all 22000 bytes.
    ///
    /// ```php
    /// $z = base64_decode('UEsDBD8AAgAOAJIQQ10RaoskYwAAAPBVAAABAAAAbAkUBQBdAAAEAAAwiAmG1B9FuCOP9NI7P2QgG4TEbe+6YngwQwLuv8NzyJHDoCyOV1cqVIHHUAW6YehSADuHGILHHs/JY6H+MavdjihXb3j28THGD/KTQG+yO57q7P//xtxAAFBLAwQUAAAAXwCSEENdEWqLJJQAAADwVQAAAQAAAHj9N3pYWgAABObWtEYCACEBDAAAAI+YQZzgVe8AU10AMIgJhtQfRbgjj/TSOz9kIBuExG3vumJ4MEMC7r/Dc8iRw6AsjldXKlSBx1AFumHoUgA7hxiCxx7PyWOh/jGr3Y4oV2949vExxg/yk0Bvsjn6GAAAANpQsC7u8a+SAAFv8KsBAAA8zWvpscRn+wIAAAAABFlaUEsDBBQAAABdAJIQQ10RaoskJwAAAPBVAAABAAAAeii1L/0AWPUAALBhIGxpbmUgb2YgdGhlIHdpdG5lc3MKAQCuq9xnE1BLAQI/Az8AAgAOAJIQQ10RaoskYwAAAPBVAAABAAAAAAAAAAAAAAC2gQAAAABsUEsBAj8DFAAAAF8AkhBDXRFqiySUAAAA8FUAAAEAAAAAAAAAAAAAALaBggAAAHhQSwECPwMUAAAAXQCSEENdEWqLJCcAAADwVQAAAQAAAAAAAAAAAAAAtoE1AQAAelBLBQYAAAAAAwADAI0AAAB7AQAAAAA=');
    /// $z[129] = chr(ord($z[129]) ^ 0x5a); file_put_contents('codec-lzma-end.zip', $z);
    /// $a = new ZipArchive; $a->open('codec-lzma-end.zip'); $f = $a->getStream('l'); $k = 0;
    /// while (($r = fread($f, 8192)) !== false && $r !== '') { $k += strlen($r); }
    /// echo "$k ", strlen($a->getFromName('l')), "\n"; $a->close();
    /// unlink('codec-lzma-end.zip');
    /// ```
    /// ```expect
    ///
    /// Warning: fread(): Zip stream error: Compressed data invalid in %s on line %d
    /// 16384%s
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Compressed data invalid in %s on line %d
    /// 0
    /// ```
    ///
    /// An LZMA entry (the oracle's, 15000 letters of `mt_srand(4)`) damaged
    /// in its second 8192-byte read, where a symbol ends on the first
    /// read's last byte: liblzma counts the symbol with the first read and
    /// delivers 13426 bytes, and lzma-rust2, which normalises the range
    /// coder early, counts it with the second and delivers one byte fewer.
    ///
    /// ```php
    /// $z = base64_decode('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');
    /// $z[31 + 8193] = chr(ord($z[31 + 8193]) ^ 0x5a); file_put_contents('codec-lzma-boundary.zip', $z);
    /// $a = new ZipArchive; $a->open('codec-lzma-boundary.zip'); $f = $a->getStream('l'); $k = 0;
    /// while (($r = fread($f, 8192)) !== false && $r !== '') { $k += strlen($r); }
    /// echo "$k ", strlen($a->getFromName('l')), "\n"; $a->close();
    /// unlink('codec-lzma-boundary.zip');
    /// ```
    /// ```expect
    ///
    /// Warning: fread(): Zip stream error: Compressed data invalid in %s on line %d
    /// 13426%s
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Compressed data invalid in %s on line %d
    /// 0
    /// ```
    ///
    /// A ZSTD frame header's window byte damaged to a window of 1280 bytes,
    /// smaller than its matches reach: libzstd's error, and ruzstd reads
    /// all 22000 bytes.
    ///
    /// ```php
    /// $z = base64_decode('UEsDBD8AAgAOAJIQQ10RaoskYwAAAPBVAAABAAAAbAkUBQBdAAAEAAAwiAmG1B9FuCOP9NI7P2QgG4TEbe+6YngwQwLuv8NzyJHDoCyOV1cqVIHHUAW6YehSADuHGILHHs/JY6H+MavdjihXb3j28THGD/KTQG+yO57q7P//xtxAAFBLAwQUAAAAXwCSEENdEWqLJJQAAADwVQAAAQAAAHj9N3pYWgAABObWtEYCACEBDAAAAI+YQZzgVe8AU10AMIgJhtQfRbgjj/TSOz9kIBuExG3vumJ4MEMC7r/Dc8iRw6AsjldXKlSBx1AFumHoUgA7hxiCxx7PyWOh/jGr3Y4oV2949vExxg/yk0Bvsjn6GAAAANpQsC7u8a+SAAFv8KsBAAA8zWvpscRn+wIAAAAABFlaUEsDBBQAAABdAJIQQ10RaoskJwAAAPBVAAABAAAAeii1L/0AWPUAALBhIGxpbmUgb2YgdGhlIHdpdG5lc3MKAQCuq9xnE1BLAQI/Az8AAgAOAJIQQ10RaoskYwAAAPBVAAABAAAAAAAAAAAAAAC2gQAAAABsUEsBAj8DFAAAAF8AkhBDXRFqiySUAAAA8FUAAAEAAAAAAAAAAAAAALaBggAAAHhQSwECPwMUAAAAXQCSEENdEWqLJCcAAADwVQAAAQAAAAAAAAAAAAAAtoE1AQAAelBLBQYAAAAAAwADAI0AAAB7AQAAAAA=');
    /// $z[345] = chr(ord($z[345]) ^ 0x5a); file_put_contents('codec-zstd-window.zip', $z);
    /// $a = new ZipArchive; $a->open('codec-zstd-window.zip'); $f = $a->getStream('z'); $k = 0;
    /// while (($r = fread($f, 8192)) !== false && $r !== '') { $k += strlen($r); }
    /// echo "$k ", strlen($a->getFromName('z')), "\n"; $a->close();
    /// unlink('codec-zstd-window.zip');
    /// ```
    /// ```expect
    ///
    /// Warning: fread(): Zip stream error: Internal error in %s on line %d
    /// 0%s
    /// Warning: ZipArchive::getFromName(): Cannot read entry: Internal error in %s on line %d
    /// 0
    /// ```
    ///
    /// The same row's other half: a ZSTD entry (the oracle's, 3000 letters
    /// of `mt_srand(1)`, its literals Huffman-coded) damaged 102 bytes in,
    /// inside its literals section. libzstd decodes on and the CRC finds
    /// the damage after all 3000 bytes; ruzstd fails the section, libzip's
    /// `Internal error` at 0.
    ///
    /// ```php
    /// $z = base64_decode('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');
    /// $z[31 + 102] = chr(ord($z[31 + 102]) ^ 0x5a); file_put_contents('codec-zstd-literals.zip', $z);
    /// $a = new ZipArchive; $a->open('codec-zstd-literals.zip'); $f = $a->getStream('z'); $k = 0;
    /// while (($r = fread($f, 8192)) !== false && $r !== '') { $k += strlen($r); }
    /// echo "$k ", strlen($a->getFromName('z')), "\n"; $a->close();
    /// unlink('codec-zstd-literals.zip');
    /// ```
    /// ```expect
    ///
    /// Warning: fread(): Zip stream error: CRC error in %s on line %d
    /// 3000%s
    /// Warning: ZipArchive::getFromName(): Cannot read entry: CRC error in %s on line %d
    /// 0
    /// ```
    fn read_some(&mut self, out: &mut [u8]) -> Result<usize, Status> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if out.is_empty() || self.end_of_stream {
            return Ok(0);
        }
        let mut done = 0;
        loop {
            let space = out.len() - done;
            self.make(space);
            let ready = self.ready();
            match self.stop {
                Some((Stop::Failed(e), at)) if at <= self.reads => {
                    let keep = self.keep.min(ready).min(space);
                    done += self.take(&mut out[done..done + keep]);
                    let (rest, space) = (ready - keep, out.len() - done);
                    if space > 0 && rest <= space {
                        self.ahead.clear();
                        self.runs.clear();
                        self.error = Some(e);
                    } else {
                        done += self.take(&mut out[done..done + rest.min(space)]);
                    }
                    break;
                }
                Some((Stop::End, at)) if at <= self.reads && ready <= space => {
                    done += self.take(&mut out[done..done + ready]);
                    self.end_of_stream = true;
                    match self.trailing() {
                        Err(e) => self.error = Some(e),
                        Ok(true) if self.check => self.error = Some(Status::new(Er::Incons)),
                        Ok(_) => {}
                    }
                    break;
                }
                _ => {}
            }
            done += self.take(&mut out[done..done + ready.min(space)]);
            if done == out.len() || self.end_of_input {
                break;
            }
            let next = self.reads + 1;
            match self.codec.feed().and_then(|f| f.end) {
                Some((at, Some(e))) if at == next => {
                    self.error = Some(e);
                    break;
                }
                Some((at, None)) if at == next => self.end_of_input = true,
                _ => {}
            }
            self.reads = next;
        }
        if done > 0 {
            return Ok(done);
        }
        self.error.map_or(Ok(0), Err)
    }
}

/// What a `zip_file_t` reads from below its CRC layer.
enum Body {
    /// Data held in memory: a string source, or none at all.
    Bytes { data: Vec<u8>, offset: usize },
    /// A window read as it is: stored data, or the raw bytes
    /// `ZIP_FL_COMPRESSED` asks for.
    Window(Window),
    /// Stored data, decrypted.
    Decrypted(Raw),
    /// Deflated or bzip2 data, decompressed as libzip decompresses it.
    Compressed(Box<Decompressor>),
    /// LZMA, XZ or ZSTD data, decompressed by a codec read whole.
    Pulled(Box<Pulled>),
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
            Body::Decrypted(raw) => read_raw(raw, buf),
            Body::Compressed(d) => d.read_some(buf),
            Body::Pulled(p) => p.read_some(buf),
        }
    }

    /// The position and length of a seekable body.
    fn span(&self) -> Option<(u64, u64)> {
        match self {
            Body::Bytes { data, offset } => Some((*offset as u64, data.len() as u64)),
            Body::Window(w) => Some((w.offset, w.len)),
            Body::Decrypted(_) | Body::Compressed(_) | Body::Pulled(_) => None,
        }
    }

    fn set_offset(&mut self, to: u64) {
        match self {
            Body::Bytes { offset, .. } => *offset = usize::try_from(to).unwrap_or(usize::MAX),
            Body::Window(w) => w.offset = to,
            Body::Decrypted(_) | Body::Compressed(_) | Body::Pulled(_) => {}
        }
    }
}

/// `struct crc_context` with `validate` set.
struct Crc {
    expect: u32,
    /// The size the layer below states at the data's end, which the data
    /// is held to; `None` over a decompressor, which states the size it
    /// made (`compress_callback`'s stat at `end_of_stream`), so the two
    /// never differ.
    size: Option<u64>,
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
    /// The general purpose bit flags, whose bits 1 and 2 say whether an
    /// LZMA entry ends with an end marker.
    pub(crate) bitflags: u16,
    /// `ZIP_CHECKCONS`, which the archive was opened with.
    pub(crate) check: bool,
}

impl Reader {
    fn new(body: Body, crc: Option<(u32, u64)>, seekable: bool) -> Reader {
        let decompressed = matches!(body, Body::Compressed(_) | Body::Pulled(_));
        Reader {
            body,
            crc: crc.map(|(expect, size)| Crc {
                expect,
                size: (!decompressed).then_some(size),
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
        zf.source_error = Some(Status::new(Er::Zipclosed));
        zf
    }

    /// A window read as it is, its CRC checked where `crc` gives one.
    pub(crate) fn window(window: Window, crc: Option<(u32, u64)>) -> Reader {
        Reader::new(Body::Window(window), crc, true)
    }

    /// `zip_source_zip_file_create` past the stat, for an unchanged
    /// entry read whole: decryption, then decompression, then the CRC,
    /// each opened as libzip opens it.
    pub(crate) fn decoded(
        window: impl FnOnce() -> Result<Window, Status>,
        l: &Layers<'_>,
    ) -> Result<Reader, Status> {
        let encryption = Encryption::of(l.encryption);
        // The password the decryption layer is opened with, where one is
        // given; a method it has no layer for is refused.
        let key = match (l.password, encryption) {
            (None, _) => None,
            (Some(p), Encryption::Pkware | Encryption::Aes(_)) => Some(p),
            (Some(_), Encryption::None | Encryption::Unknown) => {
                return Err(Status::new(Er::Encrnotsupp));
            }
        };
        let decrypt = key.is_some();
        if !matches!(l.method, 0 | 8 | 12 | 14 | 93 | 95) {
            return Err(Status::new(Er::Compnotsupp));
        }
        // The decryption layer's refusal as libzip 1.12 creates it, before
        // the empty data's shortcut: twelve bytes of PKWARE header, and
        // WinZip AES's salt, check and code.
        if decrypt
            && encryption
                .overhead()
                .is_some_and(|least| l.comp_size < least)
        {
            return Err(Status::new(Er::Opnotsupp));
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
        let window = window()?;
        let raw = match (key, encryption) {
            (Some(p), Encryption::Pkware) => Raw::pkware(window, p, l.crc.map(|c| (l.dostime, c)))?,
            (Some(p), Encryption::Aes(strength)) => Raw::aes(window, p, strength, l.comp_size)?,
            _ => Raw::Plain(window),
        };
        if l.method == 0 {
            return Ok(match raw {
                Raw::Plain(w) => Reader::new(Body::Window(w), crc, true),
                raw => Reader::new(Body::Decrypted(raw), crc, false),
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
        let feed = Feed::new(raw);
        let codec = match l.method {
            // `zip_algorithm_xz.c`'s `start`: the size is the entry's
            // where neither of general purpose bits 1 and 2 says an end
            // marker ends the data.
            14 => Codec::LzmaHeader(
                Some(feed),
                if l.bitflags & 6 == 0 {
                    l.size
                } else {
                    u64::MAX
                },
            ),
            93 => Codec::Zstd(Box::new(Zstd {
                feed,
                frame: None,
                blocks: Vec::new(),
                made: 0,
            })),
            _ => Codec::Xz(Box::new(lzma_rust2::XzReader::new(feed, true)), None),
        };
        let pulled = Pulled::new(codec, l.check);
        Ok(Reader::new(Body::Pulled(Box::new(pulled)), crc, false))
    }

    /// The window the entry is read through, where it has one.
    fn window_mut(&mut self) -> Option<&mut Window> {
        match &mut self.body {
            Body::Bytes { .. } => None,
            Body::Window(w) => Some(w),
            Body::Decrypted(raw) => Some(raw.window()),
            Body::Compressed(d) => Some(d.lower.window()),
            Body::Pulled(p) => p.codec.feed().map(|f| f.lower.window()),
        }
    }

    /// Whether the entry is read from the archive's own file, as an
    /// unchanged entry is, where an added one is read from its source.
    pub(crate) fn reads_archive(&mut self) -> bool {
        self.window_mut().is_some_and(|w| w.archive)
    }

    /// `zip_fread` through `file`, the archive's `FILE`, which its data
    /// is read through where an added file's is read through its own: it
    /// is lent to the window for the read and taken back.
    pub(crate) fn read_through(
        &mut self,
        len: usize,
        file: &mut Option<Stdio>,
    ) -> Result<Vec<u8>, Status> {
        self.lend(file);
        let got = self.read(len);
        self.lend(file);
        got
    }

    fn lend(&mut self, file: &mut Option<Stdio>) {
        if let (Some(w), Some(f)) = (self.window_mut(), file.as_mut())
            && w.archive
        {
            std::mem::swap(&mut w.file, f);
        }
    }

    /// `zip_file_is_seekable`.
    pub(crate) fn seekable(&self) -> bool {
        self.seekable
    }

    /// `crc_read` on `ZIP_SOURCE_READ`: the body's bytes, the CRC and the
    /// length held to the stat's at their end.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('seek-len-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE);
    /// $z->addFromString('a', str_repeat('a', 100)); $z->setCompressionName('a', ZipArchive::CM_STORE); $z->close();
    /// $b = file_get_contents('seek-len-witness.zip'); $c = strpos($b, "PK\1\2");
    /// file_put_contents('seek-len-witness.zip', substr_replace($b, pack('V', 120), $c + 24, 4));
    /// $z->open('seek-len-witness.zip'); $f = $z->getStream('a');
    /// var_dump(fseek($f, 50), strlen(fread($f, 8192)), fread($f, 8192));
    /// $z->close(); unlink('seek-len-witness.zip');
    /// ```
    /// ```expect
    ///
    /// Warning: fread(): Zip stream error: Zip archive inconsistent: file length in header doesn't match actual file length in %s on line %d
    /// int(0)
    /// int(50)
    /// bool(false)
    /// ```
    ///
    /// A decompressed entry's length is the decompressor's, so a central
    /// size that is not it reads no error, where a stored one's is held
    /// to it:
    ///
    /// ```php
    /// foreach ([[8, gzdeflate('hello'), 6], [8, gzdeflate('hello'), 4], [0, 'hello', 6]] as [$m, $comp, $size]) {
    ///     $local = "PK\3\4" . pack('vvvvvVVVvv', 20, 0, $m, 0, 0x21, crc32('hello'), strlen($comp), $size, 1, 0) . 'c' . $comp;
    ///     $cd = "PK\1\2" . pack('vvvvvvVVVvvvvvVV', 0x0314, 20, 0, $m, 0, 0x21, crc32('hello'), strlen($comp), $size, 1, 0, 0, 0, 0, 0x81a40000, 0) . 'c';
    ///     file_put_contents('length-witness.zip', $local . $cd . "PK\5\6" . pack('vvvvVVv', 0, 0, 1, 1, strlen($cd), strlen($local), 0));
    ///     $z = new ZipArchive; $z->open('length-witness.zip');
    ///     $s = $z->getStream('c'); $a = fread($s, 100); $b = fread($s, 100); fclose($s);
    ///     echo "$m $size: ", var_export(@$z->getFromName('c'), true), ' ', var_export($a, true), ' ', var_export($b, true), "\n"; $z->close();
    /// }
    /// unlink('length-witness.zip');
    /// ```
    /// ```expect capture
    /// 8 6: 'hello' 'hello' ''
    /// 8 4: 'hell' 'hello' ''
    ///
    /// Warning: fread(): Zip stream error: Zip archive inconsistent: file length in header doesn't match actual file length in %SCRATCH%/witness.php on line 7
    /// 0 6: false 'hello' false
    /// ```
    fn read_layer(&mut self, buf: &mut [u8]) -> Result<usize, Status> {
        let n = self.body.read_some(buf)?;
        let Some(c) = &mut self.crc else {
            return Ok(n);
        };
        if n == 0 {
            // `validate_crc` (libzip 1.12): the CRC where the whole data
            // was hashed, and the length at every end of the data.
            if c.hashed == c.position {
                c.complete = true;
                if c.hasher.clone().finalize() != c.expect {
                    return Err(Status::new(Er::Crc));
                }
            }
            if c.size.is_some_and(|size| c.position != size) {
                return Err(Status::incons(Detail::InvalidFileLength, None));
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
            self.error = Some(Status::new(Er::Inval));
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

    /// Every byte to the end, read through `file` as `read_through`
    /// reads, and the error that stopped it short.
    pub(crate) fn read_to_end(&mut self, file: &mut Option<Stdio>) -> (Vec<u8>, Option<Status>) {
        let mut out = Vec::new();
        self.lend(file);
        let fault = loop {
            match self.read(ONE_READ) {
                Ok(chunk) if chunk.is_empty() => break None,
                Ok(chunk) => out.extend_from_slice(&chunk),
                Err(status) => break Some(status),
            }
        };
        self.lend(file);
        (out, fault)
    }
}
