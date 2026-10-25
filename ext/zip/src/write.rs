//! `zip_close` re-derived (`zip_close.c`, `_zip_dirent_write`,
//! `_zip_cdir_write`): the archive written to `<name>.XXXXXX.part` beside
//! it and renamed over it, the part of the old file before the first
//! changed entry kept as it was, as libzip's `clonefile(2)` keeps it on
//! macOS; nothing written where nothing changed, and the file removed
//! where no entry survives.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::archive::{
    AFL_CREATE_OR_KEEP_FILE_FOR_EMPTY_ARCHIVE, AFL_IS_TORRENTZIP, Archive, CM_BZIP2, CM_DEFAULT,
    CM_DEFLATE, CM_LZMA, D_ATTRIBUTES, D_COMMENT, D_COMP_METHOD, D_ENCRYPTION_METHOD,
    D_EXTRA_FIELD, D_FILENAME, D_LAST_MOD, D_PASSWORD, Dirent, EM_AES_128, EM_AES_192, EM_AES_256,
    EM_NONE, EM_TRAD_PKWARE, GPBF_DATA_DESCRIPTOR, GPBF_ENCODING_UTF_8, GPBF_ENCRYPTED, Source,
    TRUNCATE, actual, is_default,
};
use crate::codec;
use crate::dostime::TORRENTZIP;
use crate::error::{Done, Er, Failed, Status};
use crate::strings::{Enc, ZStr};

/// What a running `close` asks the program: the progress callback with
/// how far it is, and the cancel callback, whose `true` cancels.
pub(crate) trait Hooks {
    fn progress(&mut self, current: f64);
    fn cancel(&mut self) -> bool;
}

/// `zip_progress_t`.
struct Progress<'h> {
    hooks: &'h mut dyn Hooks,
    precision: Option<f64>,
    cancel: bool,
    last: f64,
    start: f64,
    end: f64,
}

impl Progress<'_> {
    /// `_zip_progress_start`: `true` where the program cancelled.
    fn start(&mut self) -> bool {
        if self.precision.is_some() {
            self.last = 0.0;
            self.hooks.progress(0.0);
        }
        self.cancel && self.hooks.cancel()
    }

    /// `_zip_progress_subrange`.
    fn subrange(&mut self, start: f64, end: f64) -> bool {
        self.start = start;
        self.end = end;
        self.update(0.0)
    }

    /// `_zip_progress_update`.
    fn update(&mut self, sub: f64) -> bool {
        if let Some(precision) = self.precision {
            let current = sub.clamp(0.0, 1.0) * (self.end - self.start) + self.start;
            if current - self.last > precision || (self.last < 1.0 && current >= 1.0) {
                self.hooks.progress(current);
                self.last = current;
            }
        }
        self.cancel && self.hooks.cancel()
    }
}

/// The temporary file being written, its position and, for torrentzip's
/// central directory, a running CRC-32.
struct Out {
    file: BufWriter<File>,
    pos: u64,
    crc: Option<crc32fast::Hasher>,
}

impl Out {
    fn write(&mut self, bytes: &[u8]) -> Result<(), Status> {
        self.file
            .write_all(bytes)
            .map_err(|e| Status::of(Er::Write, e.raw_os_error().unwrap_or(0)))?;
        if let Some(crc) = &mut self.crc {
            crc.update(bytes);
        }
        self.pos += bytes.len() as u64;
        Ok(())
    }
}

/// `zip_file_attributes_t` as the sources under an entry give it.
#[derive(Default)]
struct Attributes {
    host: Option<(u8, u32)>,
    version_needed: Option<u16>,
    bits: Option<(u16, u16)>,
}

/// `ZIP_FILE_ATTRIBUTES_GENERAL_PURPOSE_BIT_FLAGS_ALLOWED_MASK`.
const BITS_ALLOWED: u16 = 0x083e;

/// `_zip_dirent_apply_attributes`.
fn apply_attributes(de: &mut Dirent, a: &Attributes) {
    if let Some((flags, mask)) = a.bits {
        let mask = mask & BITS_ALLOWED;
        de.bitflags = (de.bitflags & !mask) | (flags & mask);
    }
    if de.changed & D_ATTRIBUTES == 0
        && let Some((_, ext)) = a.host
    {
        de.ext_attrib = ext;
    }
    let is_aes = matches!(de.encryption_method, EM_AES_128 | EM_AES_192 | EM_AES_256);
    let dir = de
        .filename
        .as_ref()
        .is_some_and(|f| f.raw.last() == Some(&b'/'));
    let mut needed = if de.comp_method == CM_LZMA {
        63
    } else if is_aes {
        51
    } else if de.comp_method == CM_BZIP2 {
        46
    } else if needs_zip64(de, false) {
        45
    } else if de.comp_method == CM_DEFLATE || de.encryption_method == EM_TRAD_PKWARE || dir {
        20
    } else {
        10
    };
    if let Some(v) = a.version_needed {
        needed = needed.max(v);
    }
    de.version_needed = needed;
    let mut madeby = 63 | (de.version_madeby & 0xff00);
    if de.changed & D_ATTRIBUTES == 0
        && let Some((host, _)) = a.host
    {
        madeby = (madeby & 0xff) | (u16::from(host) << 8);
    }
    de.version_madeby = madeby;
}

/// `_zip_dirent_needs_zip64`.
fn needs_zip64(de: &Dirent, central: bool) -> bool {
    de.uncomp_size >= 0xffff_ffff
        || de.comp_size >= 0xffff_ffff
        || (central && de.offset >= 0xffff_ffff)
}

/// Each value's little-endian bytes onto the buffer, as `_zip_buffer_put_*`.
macro_rules! put {
    ($b:expr; $($v:expr),* $(,)?) => {
        $( $b.extend_from_slice(&$v.to_le_bytes()); )*
    };
}

/// The low 16 bits, as `(zip_uint16_t)` takes them.
fn u16_of(n: impl TryInto<u16>) -> u16 {
    n.try_into().unwrap_or(u16::MAX)
}

/// A 32-bit field, `0xffffffff` where Zip64 carries the value.
fn u32_of(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// `_zip_ef_utf8`: version 1, the CRC-32 of the raw string, the string.
fn ef_utf8(id: u16, s: &ZStr) -> Vec<u8> {
    let mut ef = Vec::new();
    put!(ef; id, u16_of(s.raw.len() + 5), 1u8, crc32fast::hash(&s.raw));
    ef.extend_from_slice(&s.raw);
    ef
}

/// `_zip_dirent_write`: a local header or a central record, refused
/// with libzip 1.12's `ZIP_ER_EF_TOO_LARGE` where libzip's own extra
/// fields and the entry's pass 65535 bytes.
///
/// A `WinZip` AES entry is AE-2 below 20 bytes and AE-1 from there:
///
/// ```php
/// $z = new ZipArchive; $z->open('aes-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE);
/// $z->addFromString('small', 'tiny'); $z->addFromString('big', str_repeat('b', 64));
/// foreach (['small', 'big'] as $n) $z->setEncryptionName($n, ZipArchive::EM_AES_256, 'pw');
/// $z->close(); $b = file_get_contents('aes-witness.zip');
/// $v = []; for ($at = 0; ($at = strpos($b, "\x01\x99\x07\x00", $at)) !== false; $at += 4) $v[] = 'AE-' . unpack('v', $b, $at + 4)[1];
/// echo implode(' ', $v), "\n"; $z->open('aes-witness.zip'); $z->setPassword('pw'); var_dump($z->getFromName('small'), strlen($z->getFromName('big')));
/// $z->close(); unlink('aes-witness.zip');
/// ```
/// ```expect
/// AE-2 AE-1 AE-2 AE-1
/// string(4) "tiny"
/// int(64)
/// ```
///
/// An entry whose own central field leaves no room for the AES field:
///
/// ```php
/// $ef = pack('vv', 0xcafe, 65527) . str_repeat('z', 65527);
/// $local = "PK\3\4" . pack('vvvvvVVVvv', 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 1, 0) . 'a' . 'hello';
/// $cd = "PK\1\2" . pack('vvvvvvVVVvvvvvVV', 0x0314, 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 1, strlen($ef), 0, 0, 0, 0x81a40000, 0) . 'a' . $ef;
/// file_put_contents('ef-witness.zip', $local . $cd . "PK\5\6" . pack('vvvvVVv', 0, 0, 1, 1, strlen($cd), strlen($local), 0));
/// $z = new ZipArchive; var_dump($z->open('ef-witness.zip'), $z->getFromName('a'));
/// $z->setEncryptionName('a', ZipArchive::EM_AES_256, 'pw'); var_dump($z->close(), $z->status, $z->getStatusString());
/// unlink('ef-witness.zip');
/// ```
/// ```expect
/// bool(true)
/// string(5) "hello"
///
/// Warning: ZipArchive::close(): Extra fields too large in %s on line %d
/// bool(false)
/// int(36)
/// string(22) "Extra fields too large"
/// ```
#[expect(clippy::too_many_lines, reason = "_zip_dirent_write's one body")]
fn dirent_bytes(de: &mut Dirent, central: bool, torrentzip: bool) -> Result<Vec<u8>, Status> {
    let name_enc = de.filename.as_ref().map_or(Enc::Ascii, ZStr::guess);
    let com_enc = de.comment.as_ref().map_or(Enc::Ascii, ZStr::guess);
    let mut efs: Vec<Vec<u8>> = Vec::new();
    let known = Enc::Utf8Known;
    if (name_enc == known && com_enc == Enc::Ascii)
        || (name_enc == Enc::Ascii && com_enc == known)
        || (name_enc == known && com_enc == known)
    {
        de.bitflags |= GPBF_ENCODING_UTF_8;
    } else {
        de.bitflags &= !GPBF_ENCODING_UTF_8;
        if name_enc == known
            && let Some(f) = &de.filename
        {
            efs.push(ef_utf8(0x7075, f));
        }
        if central
            && com_enc == known
            && let Some(c) = &de.comment
        {
            efs.insert(0, ef_utf8(0x6375, c));
        }
    }
    if de.encryption_method == EM_NONE {
        de.bitflags &= !GPBF_ENCRYPTED;
    } else {
        de.bitflags |= GPBF_ENCRYPTED;
    }
    let really_zip64 = needs_zip64(de, central);
    if really_zip64 {
        let mut z = Vec::new();
        if !central && (de.comp_size > 0xffff_ffff || de.uncomp_size > 0xffff_ffff) {
            put!(z; de.uncomp_size, de.comp_size);
        }
        for v in [de.uncomp_size, de.comp_size, de.offset]
            .into_iter()
            .filter(|v| central && *v >= 0xffff_ffff)
        {
            put!(z; v);
        }
        let mut ef = Vec::new();
        put!(ef; 1u16, u16_of(z.len()));
        efs.insert(0, [ef, z].concat());
    }
    let aes = matches!(de.encryption_method, EM_AES_128 | EM_AES_192 | EM_AES_256);
    if aes {
        // `winzip_aes_version` (libzip 1.12): AE-2, the CRC field zero,
        // below 20 bytes, and AE-1 from there.
        let version = if de.uncomp_size < 20 { 2 } else { 1 };
        let mut ef = vec![
            0x01,
            0x99,
            7,
            0,
            version,
            0,
            b'A',
            b'E',
            (de.encryption_method & 0xff) as u8,
        ];
        put!(ef; u16_of(de.comp_method & 0xffff));
        efs.insert(0, ef);
    }
    let own: Vec<u8> = match (torrentzip, central) {
        (true, _) => Vec::new(),
        (false, true) => de.extra_central.clone(),
        (false, false) => de.extra_local.clone().unwrap_or_default(),
    };
    let internal: Vec<u8> = efs.concat();
    let Ok(ef_len) = u16::try_from(internal.len() + own.len()) else {
        return Err(Status::of(Er::EfTooLarge, 0));
    };
    let name = de.filename.as_ref().map_or(&[][..], |f| f.raw.as_slice());
    let comment = de.comment.as_ref().map_or(&[][..], |c| c.raw.as_slice());
    let mut b = Vec::with_capacity(46 + name.len() + internal.len() + own.len());
    b.extend_from_slice(if central {
        b"PK\x01\x02"
    } else {
        b"PK\x03\x04"
    });
    if central {
        put!(b; de.version_madeby);
    }
    let needed = if really_zip64 {
        de.version_needed.max(45)
    } else {
        de.version_needed
    };
    let method = if aes { 99 } else { actual(de.comp_method) };
    let dostime = if torrentzip { TORRENTZIP } else { de.last_mod };
    let crc = if aes && de.uncomp_size < 20 {
        0
    } else {
        de.crc
    };
    put!(b; needed, de.bitflags, method, dostime.time, dostime.date, crc);
    if !central && (de.comp_size >= 0xffff_ffff || de.uncomp_size >= 0xffff_ffff) {
        put!(b; u64::MAX);
    } else {
        put!(b; u32_of(de.comp_size), u32_of(de.uncomp_size));
    }
    put!(b; u16_of(name.len()), ef_len);
    if central {
        let comment_len = if torrentzip { 0 } else { u16_of(comment.len()) };
        put!(b; comment_len, u16_of(de.disk_number), de.int_attrib, de.ext_attrib, u32_of(de.offset));
    }
    b.extend_from_slice(name);
    b.extend_from_slice(&internal);
    b.extend_from_slice(&own);
    if central && !torrentzip {
        b.extend_from_slice(comment);
    }
    Ok(b)
}

/// `write_data_descriptor`.
fn data_descriptor(de: &Dirent) -> Vec<u8> {
    let mut b = b"PK\x07\x08".to_vec();
    put!(b; de.crc);
    if needs_zip64(de, false) {
        put!(b; de.comp_size, de.uncomp_size);
    } else {
        put!(b; u32_of(de.comp_size), u32_of(de.uncomp_size));
    }
    b
}

/// `torrentzip_compare_names`: `strcasecmp`, a missing name first.
fn torrentzip_order(a: Option<&Vec<u8>>, b: Option<&Vec<u8>>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()),
        (a, b) => a.is_some().cmp(&b.is_some()),
    }
}

/// `zip_dirent_torrentzip_normalize`.
fn torrentzip_normalize(de: &mut Dirent) {
    de.version_madeby = 0;
    de.version_needed = 20;
    de.bitflags = 2;
    de.comp_method = CM_DEFLATE;
    de.compression_level = crate::codec::TORRENTZIP_COMPRESSION_FLAGS;
    de.disk_number = 0;
    de.int_attrib = 0;
    de.ext_attrib = 0;
}

impl Archive {
    /// `zip_close`: the changes written, or nothing where nothing
    /// changed; `umask` is the one a new file is created under.
    pub(crate) fn close(&mut self, hooks: &mut dyn Hooks, umask: u32) -> Done {
        let (changed, survivors) = self.changed();
        if survivors == 0 && self.ch_flags & AFL_CREATE_OR_KEEP_FILE_FOR_EMPTY_ARCHIVE == 0 {
            if (self.open_flags & TRUNCATE != 0 || changed)
                && let Err(e) = std::fs::remove_file(&self.path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                self.error = Status::of(Er::Remove, e.raw_os_error().unwrap_or(0));
                return Err(Failed);
            }
            return Ok(());
        }
        if !changed && survivors > 0 {
            return Ok(());
        }
        let torrentzip = self.want_torrentzip();
        let mut list: Vec<(usize, Option<Vec<u8>>)> = Vec::new();
        for i in 0..self.entries.len() {
            if !self.entries[i].deleted {
                let name = self.get_name(i, 0).ok();
                list.push((i, name));
            }
        }
        if torrentzip {
            list.sort_by(|a, b| torrentzip_order(a.1.as_ref(), b.1.as_ref()));
        }
        let unchanged = if torrentzip {
            0
        } else {
            self.unchanged_offset()?
        };
        let (tmp, file) = self.temp_output(umask)?;
        let mut out = Out {
            file: BufWriter::new(file),
            pos: 0,
            crc: None,
        };
        let mut progress = Progress {
            hooks,
            precision: self.progress,
            cancel: self.cancel,
            last: 0.0,
            start: 0.0,
            end: 1.0,
        };
        let written = self.write_all(&mut out, &list, unchanged, &mut progress, torrentzip);
        let flushed = written.and_then(|()| {
            out.file
                .flush()
                .map_err(|e| Status::of(Er::Write, e.raw_os_error().unwrap_or(0)))
        });
        drop(out);
        if let Err(status) = flushed {
            std::fs::remove_file(&tmp).ok();
            if status != Status::default() {
                self.error = status;
            }
            return Err(Failed);
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            std::fs::remove_file(&tmp).ok();
            self.error = Status::of(Er::Rename, e.raw_os_error().unwrap_or(0));
            return Err(Failed);
        }
        progress.update(1.0);
        Ok(())
    }

    /// The offset up to which the old file is kept: the first changed
    /// entry's, or the end of the last entry where none changed.
    fn unchanged_offset(&mut self) -> Done<u64> {
        let first = self
            .entries
            .iter()
            .filter(|e| e.has_changes())
            .filter_map(|e| e.orig.as_ref().map(|o| o.offset))
            .min();
        if let Some(offset) = first {
            return Ok(offset);
        }
        let last = (0..self.entries.len())
            .filter_map(|i| self.entries[i].orig.as_ref().map(|o| (o.offset, i)))
            .max();
        let Some((_, i)) = last else {
            return Ok(0);
        };
        let Ok((start, _)) = self.local_header(i) else {
            return self.fail(Er::Incons);
        };
        let Some(orig) = self.entries[i].orig.clone() else {
            return Ok(0);
        };
        let mut end = start + orig.comp_size;
        if orig.bitflags & GPBF_DATA_DESCRIPTOR != 0 {
            let magic = self.data.as_mut().and_then(|d| {
                d.through(|f| {
                    let mut magic = [0u8; 4];
                    f.seek(end);
                    f.read(&mut magic).ok() == Some(4) && magic == *b"PK\x07\x08"
                })
            });
            if magic == Some(true) {
                end += 4;
            }
            end += if needs_zip64(&orig, false) { 20 } else { 12 };
        }
        Ok(end)
    }

    /// `create_temp_output`: `<name>.XXXXXX.part`, the old file's permissions or
    /// `0666` under the umask.
    fn temp_output(&mut self, umask: u32) -> Done<(PathBuf, File)> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode = self
            .data
            .as_ref()
            .map_or(0o666 & !umask, |d| d.mode & 0o7777);
        let mut last = 0;
        for _ in 0..100 {
            let mut r = [0u8; 6];
            getrandom::fill(&mut r).ok();
            let suffix: String = r
                .iter()
                .map(|b| {
                    char::from(
                        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
                            [usize::from(*b) % 62],
                    )
                })
                .collect();
            let mut name = self.path.clone().into_os_string();
            name.push(format!(".{suffix}.part"));
            let tmp = PathBuf::from(name);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
            {
                Ok(f) => {
                    f.set_permissions(std::fs::Permissions::from_mode(mode))
                        .ok();
                    return Ok((tmp, f));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    last = e.raw_os_error().unwrap_or(0);
                }
                Err(e) => {
                    self.error = Status::of(Er::Tmpopen, e.raw_os_error().unwrap_or(0));
                    return Err(Failed);
                }
            }
        }
        self.error = Status::of(Er::Tmpopen, last);
        Err(Failed)
    }

    /// The body of `zip_close` past `zip_source_begin_write`.
    #[expect(
        clippy::too_many_lines,
        clippy::cast_precision_loss,
        reason = "zip_close's one loop, and its progress as libzip's doubles"
    )]
    fn write_all(
        &mut self,
        out: &mut Out,
        list: &[(usize, Option<Vec<u8>>)],
        unchanged: u64,
        progress: &mut Progress<'_>,
        torrentzip: bool,
    ) -> Result<(), Status> {
        let fail = |a: &mut Archive, er: Er| -> Status {
            a.error.set(er, 0);
            a.error
        };
        if unchanged > 0
            && let Some(d) = &mut self.data
        {
            let sys = |e: std::io::Error| Status::of(Er::Read, e.raw_os_error().unwrap_or(0));
            d.file.seek(SeekFrom::Start(0)).map_err(sys)?;
            let mut prefix = Vec::new();
            Read::by_ref(&mut d.file)
                .take(unchanged)
                .read_to_end(&mut prefix)
                .map_err(sys)?;
            // The clone of a file cut short is extended with zeros
            // (`create_temp_output_cloning`'s `ftruncate`).
            prefix.resize(usize::try_from(unchanged).unwrap_or(prefix.len()), 0);
            out.write(&prefix)?;
        }
        if progress.start() {
            return Err(fail(self, Er::Cancelled));
        }
        let survivors = list.len() as f64;
        for (j, (i, _)) in list.iter().enumerate() {
            let i = *i;
            if progress.subrange(j as f64 / survivors, (j + 1) as f64 / survivors) {
                return Err(fail(self, Er::Cancelled));
            }
            let entry = &self.entries[i];
            if entry.orig.as_ref().is_some_and(|o| o.offset < unchanged) {
                continue;
            }
            if entry.orig.is_none() && entry.source.is_none() {
                return Err(fail(self, Er::Internal));
            }
            let new_data = entry.source.is_some()
                || entry.changed(D_COMP_METHOD)
                || entry.changed(D_ENCRYPTION_METHOD)
                || (torrentzip && self.flags & AFL_IS_TORRENTZIP == 0);
            let replacing = entry.source.is_some();
            if self.read_local_ef(i).is_err() {
                return Err(self.error);
            }
            let e = &mut self.entries[i];
            let mut de = match (e.changes.take(), &e.orig) {
                (None, orig) => {
                    let mut de = orig.clone().unwrap_or_default();
                    de.changed = 0;
                    de
                }
                (Some(c), None) => c,
                (Some(c), Some(o)) => merge(c, o, replacing),
            };
            if torrentzip {
                torrentzip_normalize(&mut de);
            }
            de.offset = out.pos;
            if new_data {
                self.add_data(i, &mut de, out, progress, torrentzip)?;
            } else {
                if de.encryption_method != EM_TRAD_PKWARE {
                    de.bitflags &= !GPBF_DATA_DESCRIPTOR;
                }
                out.write(&dirent_bytes(&mut de, false, torrentzip)?)?;
                let raw = self.raw_data(i).inspect_err(|s| self.error = *s)?;
                let total = raw.len() as f64;
                let mut done = 0usize;
                for chunk in raw.chunks(8192) {
                    out.write(chunk)?;
                    done += chunk.len();
                    if progress.update(done as f64 / total) {
                        return Err(fail(self, Er::Cancelled));
                    }
                }
                if de.bitflags & GPBF_DATA_DESCRIPTOR != 0 {
                    out.write(&data_descriptor(&de))?;
                }
            }
            self.entries[i].changes = Some(de);
        }
        let offset = out.pos;
        if torrentzip {
            out.crc = Some(crc32fast::Hasher::new());
        }
        for (i, _) in list {
            let e = &mut self.entries[*i];
            let ((Some(de), _) | (None, Some(de))) = (&mut e.changes, &mut e.orig) else {
                continue;
            };
            out.write(&dirent_bytes(de, true, torrentzip)?)?;
        }
        let cdir_crc = out.crc.take().map(crc32fast::Hasher::finalize);
        let size = out.pos - offset;
        let n = list.len() as u64;
        let mut b = Vec::new();
        if n > 0xffff || offset > 0xffff_ffff || size > 0xffff_ffff {
            b.extend_from_slice(b"PK\x06\x06");
            put!(b; 44u64, 45u16, 45u16, 0u64, n, n, size, offset);
            b.extend_from_slice(b"PK\x06\x07");
            put!(b; 0u32, offset + size, 1u32);
        }
        b.extend_from_slice(b"PK\x05\x06");
        put!(b; 0u32, u16_of(n), u16_of(n), u32_of(size), u32_of(offset));
        let comment = if self.comment_changed {
            self.comment_changes.as_ref()
        } else {
            self.comment_orig.as_ref()
        };
        let comment: Vec<u8> = match cdir_crc {
            Some(crc) => format!("TORRENTZIPPED-{crc:08X}").into_bytes(),
            None => comment.map(|c| c.raw.clone()).unwrap_or_default(),
        };
        put!(b; u16_of(comment.len()));
        b.extend_from_slice(&comment);
        out.write(&b)
    }

    /// `add_data`: the entry's data from its source or its old self,
    /// decrypted, decompressed, compressed and encrypted as its dirent
    /// asks, the dirent written before it.
    #[expect(
        clippy::too_many_lines,
        clippy::cast_precision_loss,
        reason = "add_data's one body, and its progress as libzip's doubles"
    )]
    fn add_data(
        &mut self,
        i: usize,
        de: &mut Dirent,
        out: &mut Out,
        progress: &mut Progress<'_>,
        torrentzip: bool,
    ) -> Result<(), Status> {
        let has_source = self.entries[i].source.is_some();
        let (st_method, st_enc, st_crc, st_mtime, st_dostime, st_attr) = if has_source {
            let Some(s) = &self.entries[i].source else {
                return Err(Status::of(Er::Internal, 0));
            };
            let st = s.stat();
            (0u16, EM_NONE, None, st.mtime, None, s.attributes())
        } else {
            let Some(o) = &self.entries[i].orig else {
                return Err(Status::of(Er::Internal, 0));
            };
            let crc = o.crc_valid.then_some(o.crc);
            (
                actual(o.comp_method.max(0)),
                o.encryption_method,
                crc,
                0,
                Some(o.last_mod),
                None,
            )
        };
        de.bitflags &= !GPBF_DATA_DESCRIPTOR;
        if de.changed & D_COMP_METHOD == 0 && st_method != 0 {
            de.comp_method = i32::from(st_method);
        }
        if de.changed & D_LAST_MOD == 0 {
            de.last_mod = st_dostime.unwrap_or_else(|| self.zone.u2d(st_mtime));
        }
        let recompress = torrentzip || st_method != actual(de.comp_method);
        let decompress = recompress && st_method != 0;
        let reencrypt =
            recompress || de.changed & D_PASSWORD != 0 || de.encryption_method != st_enc;
        let decrypt = reencrypt && st_enc != EM_NONE;
        let encrypt = reencrypt && de.encryption_method != EM_NONE;
        if !decrypt && st_enc == EM_TRAD_PKWARE && de.changed & D_LAST_MOD != 0 {
            de.changed &= !D_LAST_MOD;
        }
        // The data and the method it is in, with its CRC where known.
        let (mut data, mut method, crc, size): (Vec<u8>, u16, u32, u64) = if has_source {
            let read = self.entries[i].source.as_mut().map(Source::read_all);
            let data = match read {
                Some(Ok(d)) => d,
                Some(Err(s)) => {
                    self.error = s;
                    return Err(s);
                }
                None => return Err(Status::of(Er::Internal, 0)),
            };
            let crc = crc32fast::hash(&data);
            let size = data.len() as u64;
            (data, 0u16, crc, size)
        } else if decrypt || decompress {
            let password = self.default_password.clone();
            let (data, fault) = self
                .decoded(i, if decrypt { password.as_deref() } else { None })
                .inspect_err(|s| self.error = *s)?;
            if let Some(fault) = fault {
                self.error = fault;
                return Err(fault);
            }
            let crc = crc32fast::hash(&data);
            let size = data.len() as u64;
            if !decompress && st_method != 0 {
                // Decrypted only: compressed again in the method it was in.
                de.comp_method = i32::from(st_method);
            }
            (data, 0, crc, size)
        } else {
            let raw = self.raw_data(i).inspect_err(|s| self.error = *s)?;
            let size = self.entries[i].orig.as_ref().map_or(0, |o| o.uncomp_size);
            (raw, st_method, st_crc.unwrap_or(0), size)
        };
        let mut attrs = Attributes {
            host: st_attr,
            ..Attributes::default()
        };
        if method == 0 && (decompress || has_source || decrypt) && actual(de.comp_method) != 0 {
            let c = codec::compress(
                &data,
                actual(de.comp_method),
                de.compression_level,
                is_default(de.comp_method),
            )
            .inspect_err(|s| self.error = *s)?;
            data = c.data;
            method = c.method;
            attrs.version_needed = Some(c.version_needed);
            attrs.bits = Some((c.bitflags, BITS_ALLOWED));
        }
        if encrypt {
            let password = de
                .password
                .clone()
                .or_else(|| self.default_password.clone());
            let Some(password) = password else {
                self.error.set(Er::Inval, 0);
                return Err(self.error);
            };
            if de.encryption_method == EM_TRAD_PKWARE {
                de.bitflags |= GPBF_DATA_DESCRIPTOR;
                data = codec::pkware(&data, &password, de.last_mod.time)?;
                attrs.version_needed = Some(20);
                attrs.bits = Some((GPBF_DATA_DESCRIPTOR, GPBF_DATA_DESCRIPTOR));
            } else {
                data = codec::winzip_aes(&data, &password, (de.encryption_method & 0xff) as u8)?;
                attrs.version_needed = Some(51);
            }
        }
        de.comp_method = i32::from(method);
        de.crc = crc;
        de.uncomp_size = size;
        de.comp_size = data.len() as u64;
        if !torrentzip {
            apply_attributes(de, &attrs);
        }
        out.write(&dirent_bytes(de, false, torrentzip)?)?;
        let total = size.max(1) as f64;
        let chunks = data.chunks(8192).count().max(1);
        for (n, chunk) in data.chunks(8192).enumerate() {
            out.write(chunk)?;
            if chunk.len() == 8192
                && n + 1 < chunks
                && progress.update(((n + 1) * 8192) as f64 / total)
            {
                self.error.set(Er::Cancelled, 0);
                return Err(self.error);
            }
        }
        if de.bitflags & GPBF_DATA_DESCRIPTOR != 0 {
            out.write(&data_descriptor(de))?;
        }
        Ok(())
    }
}

/// `_zip_dirent_merge`: what the changes did not change, from the
/// original; under new data, the default method and no encryption.
fn merge(mut de: Dirent, o: &Dirent, replacing: bool) -> Dirent {
    if de.changed & D_ATTRIBUTES == 0 {
        de.ext_attrib = o.ext_attrib;
        de.int_attrib = o.int_attrib;
    }
    if de.changed & D_COMMENT == 0 {
        de.comment.clone_from(&o.comment);
    }
    if de.changed & D_COMP_METHOD == 0 {
        (de.comp_method, de.compression_level) = if replacing {
            (CM_DEFAULT, 0)
        } else {
            (o.comp_method, o.compression_level)
        };
    }
    if de.changed & D_ENCRYPTION_METHOD == 0 {
        de.encryption_method = if replacing {
            EM_NONE
        } else {
            o.encryption_method
        };
    }
    if de.changed & D_EXTRA_FIELD == 0 {
        de.extra_central.clone_from(&o.extra_central);
        de.extra_local.clone_from(&o.extra_local);
    }
    if de.changed & D_FILENAME == 0 {
        de.filename.clone_from(&o.filename);
    }
    if de.changed & D_LAST_MOD == 0 {
        de.last_mod = o.last_mod;
    }
    if de.changed & D_PASSWORD == 0 {
        de.password.clone_from(&o.password);
    }
    de
}
