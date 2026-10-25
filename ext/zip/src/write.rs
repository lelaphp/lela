//! `zip_close` re-derived (`zip_close.c`, `_zip_dirent_write`,
//! `_zip_cdir_write`): the archive written to `<name>.XXXXXX.part` beside
//! it and renamed over it, the part of the old file before the first
//! changed entry kept as it was, as libzip's `clonefile(2)` keeps it on
//! macOS; nothing written where nothing changed, and the file removed
//! where no entry survives.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::archive::{
    AFL_CREATE_OR_KEEP_FILE_FOR_EMPTY_ARCHIVE, AFL_IS_TORRENTZIP, Archive, CM_BZIP2, CM_DEFAULT,
    CM_DEFLATE, CM_LZMA, D_ATTRIBUTES, D_COMMENT, D_COMP_METHOD, D_ENCRYPTION_METHOD,
    D_EXTRA_FIELD, D_FILENAME, D_LAST_MOD, D_PASSWORD, Dirent, EM_NONE, EM_TRAD_PKWARE, Encryption,
    GPBF_DATA_DESCRIPTOR, GPBF_ENCODING_UTF_8, GPBF_ENCRYPTED, TRUNCATE, actual, is_default,
};
use crate::cdir::Kind;
use crate::codec;
use crate::dostime::TORRENTZIP;
use crate::error::{Done, Er, Failed, Status};
use crate::strings::{Enc, ZStr};

/// What a running close reaches beside what it writes: the archive, live
/// in its holder between the callbacks as libzip's `za` stays live under
/// `zip_close` (php's `archive->za`), and the progress and cancel
/// callbacks, which may reach the archive in turn. The close reaches it
/// afresh past each callback, so no borrow of it is held across one.
pub(crate) trait Program {
    /// The archive under its close; `None` once it has no holder.
    fn archive(&mut self) -> Option<&mut Archive>;
    /// The progress callback, told how far the close is.
    fn progress(&mut self, current: f64);
    /// The cancel callback, whose `true` cancels.
    fn cancel(&mut self) -> bool;
    /// `open_count > 1` at `zip_source_commit_write`: whether a
    /// `zip_file_t` besides the close's own reads the archive's file.
    fn in_use(&mut self) -> bool;
}

/// The archive under its close, or `ZIP_ER_INTERNAL` where it has none.
fn live(program: &mut dyn Program) -> Result<&mut Archive, Status> {
    program.archive().ok_or(Status::new(Er::Internal))
}

/// What the archive has registered, read at each point as libzip reads
/// `za->progress`: the progress callback's precision, and whether a
/// cancel callback is there.
fn registered(program: &mut dyn Program) -> (Option<f64>, bool) {
    program
        .archive()
        .map_or((None, false), |za| (za.progress, za.cancel))
}

/// `ZIP_ER_CANCELLED` set on the archive, and what it holds then.
fn cancelled(program: &mut dyn Program) -> Status {
    match program.archive() {
        Some(za) => {
            za.error.set(Er::Cancelled);
            za.error
        }
        None => Status::new(Er::Cancelled),
    }
}

/// `zip_progress_t`'s state: the last value the progress callback was
/// told, and the share of the close the entry being written spans.
struct Progress {
    last: f64,
    start: f64,
    end: f64,
}

impl Progress {
    /// `_zip_progress_start`: `true` where the program cancelled.
    fn start(&mut self, program: &mut dyn Program) -> bool {
        if registered(program).0.is_some() {
            self.last = 0.0;
            program.progress(0.0);
        }
        registered(program).1 && program.cancel()
    }

    /// `_zip_progress_subrange`.
    fn subrange(&mut self, program: &mut dyn Program, start: f64, end: f64) -> bool {
        self.start = start;
        self.end = end;
        self.update(program, 0.0)
    }

    /// `_zip_progress_update`.
    fn update(&mut self, program: &mut dyn Program, sub: f64) -> bool {
        if let Some(precision) = registered(program).0 {
            let current = sub.clamp(0.0, 1.0) * (self.end - self.start) + self.start;
            if current - self.last > precision || (self.last < 1.0 && current >= 1.0) {
                program.progress(current);
                self.last = current;
            }
        }
        registered(program).1 && program.cancel()
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
            .map_err(|e| Status::os(Er::Write, &e))?;
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
    let is_aes = Encryption::of(de.encryption_method).aes().is_some();
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
    } else if needs_zip64(de, Kind::Local) {
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
fn needs_zip64(de: &Dirent, kind: Kind) -> bool {
    de.uncomp_size >= 0xffff_ffff
        || de.comp_size >= 0xffff_ffff
        || (kind == Kind::Central && de.offset >= 0xffff_ffff)
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
fn dirent_bytes(de: &mut Dirent, kind: Kind, torrentzip: bool) -> Result<Vec<u8>, Status> {
    let central = kind == Kind::Central;
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
    let really_zip64 = needs_zip64(de, kind);
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
    let aes = Encryption::of(de.encryption_method).aes();
    if let Some(strength) = aes {
        // `winzip_aes_version` (libzip 1.12): AE-2, the CRC field zero,
        // below 20 bytes, and AE-1 from there.
        let version = if de.uncomp_size < 20 { 2 } else { 1 };
        let mut ef = vec![0x01, 0x99, 7, 0, version, 0, b'A', b'E', strength];
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
        return Err(Status::new(Er::EfTooLarge));
    };
    let name = de.filename.as_ref().map_or(&[][..], |f| f.raw.as_slice());
    let comment = de.comment.as_ref().map_or(&[][..], |c| c.raw.as_slice());
    let mut b = Vec::with_capacity(46 + name.len() + internal.len() + own.len());
    b.extend_from_slice(kind.magic());
    if central {
        put!(b; de.version_madeby);
    }
    let needed = if really_zip64 {
        de.version_needed.max(45)
    } else {
        de.version_needed
    };
    let method = if aes.is_some() {
        99
    } else {
        actual(de.comp_method)
    };
    let dostime = if torrentzip { TORRENTZIP } else { de.last_mod };
    let crc = if aes.is_some() && de.uncomp_size < 20 {
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
    if needs_zip64(de, Kind::Local) {
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

/// `zip_close`: the changes written, or nothing where nothing changed;
/// `umask` is the one a new file is created under. The archive is
/// `program`'s, reached past each callback afresh, so a callback reads
/// and changes the archive being written as libzip lets it: an entry it
/// adds is not written, and a stream of the archive's own data open at
/// the commit fails it.
pub(crate) fn close(program: &mut dyn Program, umask: u32) -> Done {
    let Some(za) = program.archive() else {
        return Err(Failed);
    };
    let (changed, survivors) = za.changed();
    if survivors == 0 && za.ch_flags & AFL_CREATE_OR_KEEP_FILE_FOR_EMPTY_ARCHIVE == 0 {
        if (za.open_flags & TRUNCATE != 0 || changed)
            && let Err(e) = std::fs::remove_file(&za.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            za.error = Status::os(Er::Remove, &e);
            return Err(Failed);
        }
        return Ok(());
    }
    if !changed && survivors > 0 {
        return Ok(());
    }
    let torrentzip = za.want_torrentzip();
    let mut list: Vec<(usize, Option<Vec<u8>>)> = Vec::new();
    for i in 0..za.entries.len() {
        if !za.entries[i].deleted {
            let name = za.get_name(i, 0).ok();
            list.push((i, name));
        }
    }
    if torrentzip {
        list.sort_by(|a, b| torrentzip_order(a.1.as_ref(), b.1.as_ref()));
    }
    let unchanged = if torrentzip {
        0
    } else {
        za.unchanged_offset()?
    };
    let (tmp, file) = za.temp_output(umask)?;
    let mut out = Out {
        file: BufWriter::new(file),
        pos: 0,
        crc: None,
    };
    let mut progress = Progress {
        last: 0.0,
        start: 0.0,
        end: 1.0,
    };
    let committed = match write_all(
        program,
        &mut out,
        &list,
        unchanged,
        &mut progress,
        torrentzip,
    ) {
        Ok(()) => {
            let committed = commit(program, out, &tmp);
            // `_zip_progress_end`, past the commit whether it took or not.
            progress.update(program, 1.0);
            committed
        }
        Err(status) => {
            drop(out);
            Err(status)
        }
    };
    let Err(status) = committed else {
        return Ok(());
    };
    std::fs::remove_file(&tmp).ok();
    if status != Status::default()
        && let Some(za) = program.archive()
    {
        za.error = status;
    }
    Err(Failed)
}

/// `zip_source_commit_write`: `ZIP_ER_INUSE` while a `zip_file_t`
/// besides the close's own reads the archive's file, then the temporary
/// file flushed and renamed over the archive.
fn commit(program: &mut dyn Program, mut out: Out, tmp: &Path) -> Result<(), Status> {
    if program.in_use() {
        return Err(Status::new(Er::Inuse));
    }
    out.file.flush().map_err(|e| Status::os(Er::Write, &e))?;
    drop(out);
    let path = live(program)?.path.clone();
    std::fs::rename(tmp, &path).map_err(|e| Status::os(Er::Rename, &e))
}

/// The body of `zip_close` past `zip_source_begin_write`: the kept part
/// of the old file, each surviving entry, and the central directory.
#[expect(
    clippy::cast_precision_loss,
    reason = "zip_close's progress as libzip's doubles"
)]
fn write_all(
    program: &mut dyn Program,
    out: &mut Out,
    list: &[(usize, Option<Vec<u8>>)],
    unchanged: u64,
    progress: &mut Progress,
    torrentzip: bool,
) -> Result<(), Status> {
    if unchanged > 0
        && let Some(d) = &mut live(program)?.data
    {
        let sys = |e: std::io::Error| Status::os(Er::Read, &e);
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
    if progress.start(program) {
        return Err(cancelled(program));
    }
    let survivors = list.len() as f64;
    for (j, (i, _)) in list.iter().enumerate() {
        if progress.subrange(program, j as f64 / survivors, (j + 1) as f64 / survivors) {
            return Err(cancelled(program));
        }
        let Some(entry) = live(program)?.entry(*i, out.pos, unchanged, torrentzip)? else {
            continue;
        };
        out.write(&entry.header)?;
        let total = entry.data.len() as f64;
        let chunks = entry.data.chunks(8192).count();
        let mut done = 0usize;
        for (n, chunk) in entry.data.chunks(8192).enumerate() {
            out.write(chunk)?;
            done += chunk.len();
            let at = match entry.pace {
                Pace::Copy => Some(done as f64 / total),
                Pace::Add { size } => {
                    (chunk.len() == 8192 && n + 1 < chunks).then(|| done as f64 / size)
                }
            };
            if at.is_some_and(|at| progress.update(program, at)) {
                return Err(cancelled(program));
            }
        }
        if let Some(descriptor) = &entry.descriptor {
            out.write(descriptor)?;
        }
    }
    live(program)?.write_cdir(out, list, torrentzip)
}

/// One surviving entry as `zip_close`'s loop writes it: its local header,
/// its data and the `write_data_descriptor` record after them.
struct Written {
    header: Vec<u8>,
    data: Vec<u8>,
    pace: Pace,
    descriptor: Option<Vec<u8>>,
}

/// When the progress callback hears of an entry's data written.
#[derive(Clone, Copy)]
enum Pace {
    /// `copy_data`: after each buffer, as the share of the data written.
    Copy,
    /// `add_data`: after each full buffer but the last, as the share of
    /// the uncompressed `size` the buffers so far make.
    Add { size: f64 },
}

impl Archive {
    /// One turn of `zip_close`'s loop for entry `i`, written at `offset`:
    /// its dirent made of its changes and its original, kept as its
    /// changes before its data is written, so a callback that changes the
    /// entry meanwhile changes what the central directory says of it;
    /// `None` for an entry the kept part of the old file holds.
    fn entry(
        &mut self,
        i: usize,
        offset: u64,
        unchanged: u64,
        torrentzip: bool,
    ) -> Result<Option<Written>, Status> {
        let Some(entry) = self.entries.get(i) else {
            self.error.set(Er::Internal);
            return Err(self.error);
        };
        if entry.orig.as_ref().is_some_and(|o| o.offset < unchanged) {
            return Ok(None);
        }
        if entry.orig.is_none() && entry.source.is_none() {
            self.error.set(Er::Internal);
            return Err(self.error);
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
        de.offset = offset;
        let written = if new_data {
            self.add_data(i, &mut de, torrentzip)
        } else {
            self.copied(i, &mut de, torrentzip)
        };
        self.entries[i].changes = Some(de);
        written.map(Some)
    }

    /// An entry whose data is copied as it stands: its local header
    /// written anew, with the sizes after the data dropped but under
    /// PKWARE's encryption, whose check reads them.
    fn copied(&mut self, i: usize, de: &mut Dirent, torrentzip: bool) -> Result<Written, Status> {
        if de.encryption_method != EM_TRAD_PKWARE {
            de.bitflags &= !GPBF_DATA_DESCRIPTOR;
        }
        let header = dirent_bytes(de, Kind::Local, torrentzip)?;
        let data = self.raw_data(i).inspect_err(|s| self.error = *s)?;
        Ok(Written {
            header,
            data,
            pace: Pace::Copy,
            descriptor: (de.bitflags & GPBF_DATA_DESCRIPTOR != 0).then(|| data_descriptor(de)),
        })
    }

    /// `write_cdir`: the central directory, then the Zip64 end records
    /// where they are needed, then the end record with the comment, or
    /// torrentzip's in its place.
    fn write_cdir(
        &mut self,
        out: &mut Out,
        list: &[(usize, Option<Vec<u8>>)],
        torrentzip: bool,
    ) -> Result<(), Status> {
        let offset = out.pos;
        if torrentzip {
            out.crc = Some(crc32fast::Hasher::new());
        }
        for (i, _) in list {
            let Some(e) = self.entries.get_mut(*i) else {
                continue;
            };
            let ((Some(de), _) | (None, Some(de))) = (&mut e.changes, &mut e.orig) else {
                continue;
            };
            out.write(&dirent_bytes(de, Kind::Central, torrentzip)?)?;
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
        let start = match self.data_offset(i) {
            Ok(start) => start,
            Err(status) => {
                self.error = status;
                return Err(Failed);
            }
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
            end += if needs_zip64(&orig, Kind::Local) {
                20
            } else {
                12
            };
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
                    self.error = Status::os(Er::Tmpopen, &e);
                    return Err(Failed);
                }
            }
        }
        self.error = Status::errno(Er::Tmpopen, last);
        Err(Failed)
    }

    /// `add_data`: the entry's data from its source or its old self,
    /// decrypted, decompressed, compressed and encrypted as its dirent
    /// asks, the dirent written before it. An unchanged entry is read as
    /// libzip's zip source reads it, decrypted and decompressed, so any
    /// method but store compresses it again, at the level its dirent asks:
    ///
    /// ```php
    /// $d = ''; for ($i = 0; $i < 4000; $i++) $d .= md5((string) ($i % 97)) . ' ';
    /// $z = new ZipArchive; $z->open('rc-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE);
    /// $z->addFromString('s', str_repeat('a', 999)); $z->setCompressionName('s', ZipArchive::CM_STORE);
    /// $z->addFromString('l', $d); $z->setCompressionName('l', ZipArchive::CM_DEFLATE, 9);
    /// $z->addFromString('e', $d); $z->setCompressionName('e', ZipArchive::CM_DEFLATE, 1); $z->close();
    /// $z->open('rc-witness.zip'); $z->setCompressionName('s', ZipArchive::CM_DEFLATE);
    /// $z->setCompressionName('l', ZipArchive::CM_DEFLATE, 1); $z->setEncryptionName('e', ZipArchive::EM_TRAD_PKWARE, 'pw'); $z->close();
    /// $z->open('rc-witness.zip'); $z->setPassword('pw');
    /// foreach (['s', 'l', 'e'] as $n) { $s = $z->statName($n); echo "$n {$s['comp_method']} {$s['comp_size']} ", strlen($z->getFromName($n)), "\n"; }
    /// $z->close(); unlink('rc-witness.zip');
    /// ```
    /// ```expect
    /// s 8 11 999
    /// l 8 3071 132000
    /// e 8 2837 132000
    /// ```
    #[expect(
        clippy::cast_precision_loss,
        reason = "add_data's progress as libzip's doubles"
    )]
    fn add_data(&mut self, i: usize, de: &mut Dirent, torrentzip: bool) -> Result<Written, Status> {
        // The data as `add_data`'s source gives it: a new source's bytes,
        // or the unchanged entry through `zip_source_zip_file_create(za, i,
        // ZIP_FL_UNCHANGED, ...)`, decrypted with the default password,
        // decompressed and its CRC checked. Either stats as stored and
        // unencrypted.
        let (data, st_mtime, st_dostime, st_attr) = if let Some(s) = &mut self.entries[i].source {
            let (mtime, attr) = (s.stat().mtime, s.attributes());
            let data = s.read_all().inspect_err(|s| self.error = *s)?;
            (data, mtime, None, attr)
        } else {
            let Some(last_mod) = self.entries[i].orig.as_ref().map(|o| o.last_mod) else {
                return Err(Status::new(Er::Internal));
            };
            let password = self.default_password.clone();
            let (data, fault) = self
                .decoded(i, password.as_deref())
                .inspect_err(|s| self.error = *s)?;
            if let Some(fault) = fault {
                self.error = fault;
                return Err(fault);
            }
            (data, 0, Some(last_mod), None)
        };
        de.bitflags &= !GPBF_DATA_DESCRIPTOR;
        if de.changed & D_LAST_MOD == 0 {
            de.last_mod = st_dostime.unwrap_or_else(|| self.zone.u2d(st_mtime));
        }
        // `needs_compress`: what is stored needs no decompression, and a
        // method other than store is a recompression (`needs_recompress`).
        let compress = actual(de.comp_method) != 0;
        let encrypt = de.encryption_method != EM_NONE;
        let crc = crc32fast::hash(&data);
        let size = data.len() as u64;
        let (mut data, mut method) = (data, 0u16);
        let mut attrs = Attributes {
            host: st_attr,
            ..Attributes::default()
        };
        if compress {
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
                self.error.set(Er::Inval);
                return Err(self.error);
            };
            match Encryption::of(de.encryption_method) {
                Encryption::Pkware => {
                    de.bitflags |= GPBF_DATA_DESCRIPTOR;
                    data = codec::pkware(&data, &password, de.last_mod.time)?;
                    attrs.version_needed = Some(20);
                    attrs.bits = Some((GPBF_DATA_DESCRIPTOR, GPBF_DATA_DESCRIPTOR));
                }
                Encryption::Aes(strength) => {
                    data = codec::winzip_aes(&data, &password, strength)?;
                    attrs.version_needed = Some(51);
                }
                Encryption::None | Encryption::Unknown => {
                    self.error.set(Er::Encrnotsupp);
                    return Err(self.error);
                }
            }
        }
        de.comp_method = i32::from(method);
        de.crc = crc;
        de.uncomp_size = size;
        de.comp_size = data.len() as u64;
        if !torrentzip {
            apply_attributes(de, &attrs);
        }
        Ok(Written {
            header: dirent_bytes(de, Kind::Local, torrentzip)?,
            data,
            pace: Pace::Add {
                size: size.max(1) as f64,
            },
            descriptor: (de.bitflags & GPBF_DATA_DESCRIPTOR != 0).then(|| data_descriptor(de)),
        })
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
