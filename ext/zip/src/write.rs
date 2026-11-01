//! `zip_close` re-derived (`zip_close.c`, `_zip_dirent_write`,
//! `_zip_cdir_write`): the archive written to `<name>.XXXXXX.part` beside
//! it and renamed over it, the part of the old file before the first
//! changed entry kept as it was, as libzip's `clonefile(2)` keeps it on
//! macOS; nothing written where nothing changed, and the file removed
//! where no entry survives.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::archive::Input;
use crate::archive::{
    AFL_CREATE_OR_KEEP_FILE_FOR_EMPTY_ARCHIVE, AFL_IS_TORRENTZIP, Archive, CM_BZIP2, CM_DEFAULT,
    CM_DEFLATE, CM_LZMA, CM_STORE, D_ATTRIBUTES, D_COMMENT, D_COMP_METHOD, D_ENCRYPTION_METHOD,
    D_EXTRA_FIELD, D_FILENAME, D_LAST_MOD, D_PASSWORD, Dirent, EM_NONE, EM_TRAD_PKWARE, Encryption,
    GPBF_DATA_DESCRIPTOR, GPBF_ENCODING_UTF_8, GPBF_ENCRYPTED, TRUNCATE, actual, is_default,
};
use crate::cdir::Kind;
use crate::codec::{Attributes, BITS_ALLOWED, BUFSIZE, Compressor, Method, Sealer};
use crate::dostime::{DosTime, TORRENTZIP};
use crate::error::{Done, Er, Failed, Status};
use crate::pipe::Pipe;
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

    /// The archive under its close, or `ZIP_ER_INTERNAL` where it has
    /// none.
    fn live(&mut self) -> Result<&mut Archive, Status> {
        self.archive().ok_or(Status::new(Er::Internal))
    }

    /// The registered progress callback's precision, read at each point
    /// as libzip reads `za->progress`; `None` where none is registered.
    fn precision(&mut self) -> Option<f64> {
        self.archive().and_then(|za| za.progress)
    }

    /// Whether a cancel callback is registered, read at each point.
    fn may_cancel(&mut self) -> bool {
        self.archive().is_some_and(|za| za.cancel)
    }

    /// `ZIP_ER_CANCELLED` set on the archive, and what it holds then.
    fn cancelled(&mut self) -> Status {
        match self.archive() {
            Some(za) => {
                za.error.set(Er::Cancelled);
                za.error
            }
            None => Status::new(Er::Cancelled),
        }
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
        if program.precision().is_some() {
            self.last = 0.0;
            program.progress(0.0);
        }
        program.may_cancel() && program.cancel()
    }

    /// `_zip_progress_subrange`.
    fn subrange(&mut self, program: &mut dyn Program, start: f64, end: f64) -> bool {
        self.start = start;
        self.end = end;
        self.update(program, 0.0)
    }

    /// `_zip_progress_update`.
    fn update(&mut self, program: &mut dyn Program, sub: f64) -> bool {
        if let Some(precision) = program.precision() {
            let current = sub.clamp(0.0, 1.0) * (self.end - self.start) + self.start;
            if current - self.last > precision || (self.last < 1.0 && current >= 1.0) {
                program.progress(current);
                self.last = current;
            }
        }
        program.may_cancel() && program.cancel()
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

    /// `bytes` written again at `at`, before where the file stands, then
    /// the file back where it stood, as `add_data`'s
    /// `zip_source_seek_write` there and back writes a header again.
    fn rewrite(&mut self, at: u64, bytes: &[u8]) -> Result<(), Status> {
        let seek = |e: std::io::Error| Status::os(Er::Seek, &e);
        self.file.seek(SeekFrom::Start(at)).map_err(seek)?;
        self.file
            .write_all(bytes)
            .map_err(|e| Status::os(Er::Write, &e))?;
        self.file.seek(SeekFrom::Start(self.pos)).map_err(seek)?;
        Ok(())
    }
}

impl Attributes {
    /// An unchanged entry's source, `zip_source_zip_file_create` over its
    /// original dirent `orig`: its window's, the entry's own host,
    /// attributes and bits (`_zip_file_attributes_from_dirent`), under
    /// the decompression layer a compressed entry is read through. So the
    /// version a decompressor needs stands, its bits clear the entry's
    /// level, and a stored entry's `ZIP_GPBF_DATA_DESCRIPTOR` stays:
    ///
    /// ```php
    /// $d = str_repeat('hello world ', 2000);
    /// function hdr($label) { $b = file_get_contents('attr-witness.zip'); $l = unpack('vneed/vbits/vmethod', $b, 4);
    ///     echo "$label: need {$l['need']} bits ", dechex($l['bits']), " method {$l['method']} descriptor ", var_export(strpos($b, "PK\7\x08") !== false, true), "\n"; }
    /// $z = new ZipArchive;
    /// foreach ([[ZipArchive::CM_BZIP2, 0, ZipArchive::CM_STORE], [ZipArchive::CM_DEFLATE, 9, ZipArchive::CM_STORE], [ZipArchive::CM_XZ, 0, ZipArchive::CM_STORE], [ZipArchive::CM_BZIP2, 0, ZipArchive::CM_DEFLATE]] as [$m1, $level, $m2]) {
    ///     $z->open('attr-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE); $z->addFromString('a', $d); $z->setCompressionName('a', $m1, $level); $z->close(); hdr("$m1/$level");
    ///     $z->open('attr-witness.zip'); $z->setCompressionName('a', $m2); $z->close(); hdr("  then $m2");
    /// }
    /// foreach ([ZipArchive::EM_NONE, ZipArchive::EM_AES_256] as $em) {
    ///     $z->open('attr-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE); $z->addFromString('a', $d); $z->setCompressionName('a', ZipArchive::CM_STORE); $z->setEncryptionName('a', ZipArchive::EM_TRAD_PKWARE, 'pw'); $z->close();
    ///     $z->open('attr-witness.zip'); $z->setPassword('pw'); $z->setEncryptionName('a', $em, 'pw'); $z->close(); hdr("pkware then $em");
    ///     $z->open('attr-witness.zip'); $z->setPassword('pw'); echo '  ', strlen($z->getFromName('a')), "\n"; $z->close();
    /// }
    /// unlink('attr-witness.zip');
    /// ```
    /// ```expect
    /// 12/0: need 46 bits 0 method 12 descriptor false
    ///   then 0: need 46 bits 0 method 0 descriptor false
    /// 8/9: need 20 bits 2 method 8 descriptor false
    ///   then 0: need 20 bits 0 method 0 descriptor false
    /// 95/0: need 20 bits 0 method 95 descriptor false
    ///   then 0: need 20 bits 0 method 0 descriptor false
    /// 12/0: need 46 bits 0 method 12 descriptor false
    ///   then 8: need 46 bits 2 method 8 descriptor false
    /// pkware then 0: need 10 bits 8 method 0 descriptor true
    ///   24000
    /// pkware then 259: need 51 bits 9 method 99 descriptor true
    ///   24000
    /// ```
    fn unchanged(orig: &Dirent) -> Attributes {
        let window = Attributes {
            host: Some((
                u8::try_from(orig.version_madeby >> 8).unwrap_or(0),
                orig.ext_attrib,
            )),
            version_needed: None,
            bits: Some((orig.bitflags, BITS_ALLOWED)),
        };
        match Method::of(actual(orig.comp_method)) {
            Some(method) => Attributes::codec(method, 0).over(window),
            None => window,
        }
    }
}

/// `_zip_dirent_apply_attributes`: whether it changed the dirent.
fn apply_attributes(de: &mut Dirent, a: &Attributes, force_zip64: bool) -> bool {
    // What `_zip_dirent_apply_attributes` may write, read before and after.
    let fields = |de: &Dirent| {
        (
            de.bitflags,
            de.ext_attrib,
            de.version_needed,
            de.version_madeby,
        )
    };
    let before = fields(de);
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
    } else if force_zip64 || needs_zip64(de, Kind::Local) {
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
    before != fields(de)
}

/// `_zip_dirent_needs_zip64`.
fn needs_zip64(de: &Dirent, kind: Kind) -> bool {
    de.uncomp_size >= 0xffff_ffff
        || de.comp_size >= 0xffff_ffff
        || (kind == Kind::Central && de.offset >= 0xffff_ffff)
}

/// Whether `_zip_dirent_write` writes a Zip64 field, which it answers:
/// where the dirent needs one, and in a local header `add_data` asks for
/// one with `ZIP_FL_FORCE_ZIP64` (`force`). An entry whose stated size
/// is `0xffffffff` with no Zip64 field, recompressed by the close, has
/// one forced, so its header written again keeps it; stored, it has one
/// only as its stated size needs, which its header written again does
/// not, and the close fails as `add_data` fails it:
///
/// ```php
/// function crafted($f, $comp, $size, $crc) {
///     $local = "PK\3\4" . pack('vvvvvVVVvv', 20, 0, 8, 0, 0x21, $crc, strlen($comp), $size, 1, 0) . 'c' . $comp;
///     $cd = "PK\1\2" . pack('vvvvvvVVVvvvvvVV', 0x0314, 20, 0, 8, 0, 0x21, $crc, strlen($comp), $size, 1, 0, 0, 0, 0, 0x81a40000, 0) . 'c';
///     file_put_contents($f, $local . $cd . "PK\5\6" . pack('vvvvVVv', 0, 0, 1, 1, strlen($cd), strlen($local), 0));
/// }
/// foreach ([ZipArchive::CM_STORE, ZipArchive::CM_DEFLATE] as $to) {
///     crafted('zip64-witness.zip', gzdeflate('hello'), 0xFFFFFFFF, crc32('hello'));
///     $z = new ZipArchive; $z->open('zip64-witness.zip'); $z->setCompressionName('c', $to);
///     $r = @$z->close(); echo "$to: ", var_export($r, true), " $z->status ", $z->getStatusString(), "\n";
///     if ($r) { echo '  ', bin2hex(substr(file_get_contents('zip64-witness.zip'), 4, 46)), "\n"; $z->open('zip64-witness.zip'); var_dump($z->getFromName('c')); $z->close(); }
/// }
/// unlink('zip64-witness.zip');
/// ```
/// ```expect
/// 0: false 20 Internal error
/// 8: true 0 No error
///   2d00020008000000210086a610360700000005000000010014006301001000050000000000000007000000000000
/// string(5) "hello"
/// ```
fn is_zip64(de: &Dirent, kind: Kind, force: bool) -> bool {
    (kind == Kind::Local && force) || needs_zip64(de, kind)
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
fn dirent_bytes(
    de: &mut Dirent,
    kind: Kind,
    torrentzip: bool,
    force: bool,
) -> Result<Vec<u8>, Status> {
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
    if is_zip64(de, kind, force) {
        let mut z = Vec::new();
        let past = de.comp_size > 0xffff_ffff || de.uncomp_size > 0xffff_ffff;
        if !central && (force || past) {
            put!(z; de.uncomp_size, de.comp_size);
        }
        if central && (force || past || de.offset > 0xffff_ffff) {
            for v in [de.uncomp_size, de.comp_size, de.offset] {
                if v >= 0xffff_ffff {
                    put!(z; v);
                }
            }
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

/// `write_data_descriptor`, with Zip64 sizes where the local header has
/// a Zip64 field (`zip64`).
fn data_descriptor(de: &Dirent, zip64: bool) -> Vec<u8> {
    let mut b = b"PK\x07\x08".to_vec();
    put!(b; de.crc);
    if zip64 {
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
    // The order and the kept part of the old file are fixed here, before
    // the first callback, as `zip_close` fixes them before
    // `_zip_progress_start`; each later point reads `ZIP_WANT_TORRENTZIP`
    // afresh, so a callback that sets it torrentzips what follows.
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
    let committed = match write_all(program, &mut out, &list, unchanged, &mut progress) {
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
    if let Some(za) = program.archive() {
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
    let path = program.live()?.path.clone();
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
) -> Result<(), Status> {
    if unchanged > 0
        && let Some(d) = &mut program.live()?.data
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
        return Err(program.cancelled());
    }
    let survivors = list.len() as f64;
    for (j, (i, _)) in list.iter().enumerate() {
        if progress.subrange(program, j as f64 / survivors, (j + 1) as f64 / survivors) {
            return Err(program.cancelled());
        }
        let at = out.pos;
        let Some(mut entry) = program.live()?.entry(*i, at, unchanged)? else {
            continue;
        };
        out.write(&entry.header)?;
        match &mut entry.data {
            Data::Copy(data) => {
                let total = data.len() as f64;
                let mut done = 0usize;
                for chunk in data.chunks(BUFSIZE) {
                    out.write(chunk)?;
                    done += chunk.len();
                    if progress.update(program, done as f64 / total) {
                        return Err(program.cancelled());
                    }
                }
            }
            Data::Add(added) => {
                let mut buf = [0u8; BUFSIZE];
                loop {
                    let za = program.live()?;
                    let n = added
                        .pipe
                        .read(za, &mut buf)
                        .inspect_err(|s| za.error = *s)?;
                    if n == 0 {
                        break;
                    }
                    out.write(&buf[..n])?;
                    if n == BUFSIZE
                        && let Some(share) = added.pipe.share(program.live()?)
                        && progress.update(program, share)
                    {
                        return Err(program.cancelled());
                    }
                }
            }
        }
        program.live()?.finish(*i, &entry, at, out)?;
    }
    program.live()?.write_cdir(out, list)
}

/// One surviving entry as `zip_close`'s loop writes it before its data's
/// progress points: its local header, and its data and the way it comes.
struct Written {
    header: Vec<u8>,
    data: Data,
}

/// How an entry's data reaches the file, which says when the progress
/// callback hears of it and what follows it.
enum Data {
    /// `copy_data`: copied as it stands, the progress callback told after
    /// each buffer the share of the data written.
    Copy(Vec<u8>),
    /// `add_data`: read through the layers over its source, the progress
    /// callback told after each full buffer how far the source has been
    /// read (`copy_source`); the dirent settled after.
    Add(Box<Added>),
}

/// What `add_data` copies from: its source under its layers, and how the
/// source's end is held.
struct Added {
    pipe: Pipe,
    end: End,
    zip64: Zip64,
}

/// The Zip64 field of an added entry's local header: whether `add_data`
/// asked for one whatever the sizes (`ZIP_FL_FORCE_ZIP64`), and whether
/// the preliminary header had one, as the header written again must.
#[derive(Clone, Copy)]
struct Zip64 {
    forced: bool,
    written: bool,
}

impl Added {
    /// `zip_source_stat` of `src_final` once the data is written, and the
    /// attributes it gives then.
    fn settled(&self) -> Settled {
        let (crc, size) = self.pipe.crc_and_size();
        Settled {
            method: self.pipe.method(),
            crc,
            size,
            comp_size: self.pipe.written(),
            attributes: self.pipe.attributes(),
            zip64: self.zip64,
        }
    }
}

/// What `add_data`'s `zip_source_stat` of its final source says once the
/// data is written, and the attributes that source gives.
struct Settled {
    method: u16,
    crc: u32,
    size: u64,
    comp_size: u64,
    attributes: Attributes,
    zip64: Zip64,
}

impl Settled {
    /// The dirent given the method, CRC and sizes the data settled on, and
    /// whether any differed from what it held: the first part of
    /// `add_data`'s `dirent_changed`.
    fn settle(&self, de: &mut Dirent) -> bool {
        let changed = actual(de.comp_method) != self.method
            || de.crc != self.crc
            || de.uncomp_size != self.size
            || de.comp_size != self.comp_size;
        de.comp_method = i32::from(self.method);
        de.crc = self.crc;
        de.uncomp_size = self.size;
        de.comp_size = self.comp_size;
        changed
    }
}

impl Archive {
    /// One turn of `zip_close`'s loop for entry `i`, written at `offset`:
    /// its dirent made of its changes and its original, kept as its
    /// changes before its data is written, so a callback that changes the
    /// entry meanwhile changes what the central directory says of it;
    /// `None` for an entry the kept part of the old file holds.
    /// `ZIP_WANT_TORRENTZIP` is read as the turn starts, past the callbacks
    /// before it, and stands until its data's first progress point.
    /// A progress callback that asks for torrentzip from the close's first
    /// point torrentzips every entry, in the order the close fixed before
    /// it; one that asks during an entry's data leaves that entry's
    /// method, level and attributes as they were and writes its local
    /// header again under torrentzip's time:
    ///
    /// ```php
    /// $d = ''; for ($i = 0; $i < 3000; $i++) $d .= md5((string) $i);
    /// foreach ([0.0, 0.3] as $when) {
    ///     $z = new ZipArchive; $z->open('tz-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE);
    ///     $z->addFromString('b', $d); $z->setCompressionName('b', ZipArchive::CM_STORE); $z->addFromString('a', 'world'); $z->setMtimeName('b', 315532800); $z->setMtimeName('a', 315532800);
    ///     $z->registerProgressCallback(0.01, function ($r) use ($z, $when) { static $done; if (!$done && $r >= $when) { $done = 1; echo "set at ", $r > 0 ? 'data' : 'start', "\n"; $z->setArchiveFlag(ZipArchive::AFL_WANT_TORRENTZIP, 1); } });
    ///     var_dump($z->close()); $b = file_get_contents('tz-witness.zip');
    ///     for ($at = 0; ($at = strpos($b, "PK\3\4", $at)) !== false; $at += 4) echo 'L ', bin2hex(substr($b, $at + 4, 26)), "\n";
    ///     for ($at = 0; ($at = strpos($b, "PK\1\2", $at)) !== false; $at += 4) echo 'C ', bin2hex(substr($b, $at + 4, 42)), "\n";
    ///     echo substr($b, -22), "\n";
    ///     $z->open('tz-witness.zip'); var_dump($z->getArchiveFlag(ZipArchive::AFL_IS_TORRENTZIP), strlen($z->getFromName('b')), $z->getFromName('a')); $z->close();
    /// }
    /// unlink('tz-witness.zip');
    /// ```
    /// ```expect capture
    /// set at start
    /// bool(true)
    /// L 14000200080000bc982130c2a217b1d700000077010001000000
    /// L 14000200080000bc98214311773a070000000500000001000000
    /// C 000014000200080000bc982130c2a217b1d7000000770100010000000000000000000000000000000000
    /// C 000014000200080000bc98214311773a07000000050000000100000000000000000000000000d0d70000
    /// TORRENTZIPPED-1D9BC27F
    /// int(1)
    /// int(96000)
    /// string(5) "world"
    /// set at data
    /// bool(true)
    /// L 0a000000000000bc982130c2a217007701000077010001000000
    /// L 14000200080000bc98214311773a070000000500000001000000
    /// C 3f030a000000000000bc982130c2a2170077010000770100010000000000000000000000b68100000000
    /// C 000014000200080000bc98214311773a070000000500000001000000000000000000000000001f770100
    /// TORRENTZIPPED-FEA2E01B
    /// int(1)
    /// int(96000)
    /// string(5) "world"
    /// ```
    fn entry(&mut self, i: usize, offset: u64, unchanged: u64) -> Result<Option<Written>, Status> {
        let torrentzip = self.want_torrentzip();
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
        let header = dirent_bytes(de, Kind::Local, torrentzip, false)?;
        let data = self.raw_data(i).inspect_err(|s| self.error = *s)?;
        Ok(Written {
            header,
            data: Data::Copy(data),
        })
    }

    /// What follows entry `i`'s data, `written` at `at`, made of its
    /// dirent as the callbacks during the data left it, as libzip's `de` is
    /// `entry->changes`: the error the end of an added entry's data found,
    /// as libzip finds it past the data's last progress point, then the
    /// entry's dirent settled on its data, its local header written again
    /// where `add_data`'s `dirent_changed` holds, then the
    /// `write_data_descriptor` record where the dirent's bits ask for one.
    /// `ZIP_WANT_TORRENTZIP` is read past the data's callbacks, as
    /// `add_data` reads it there.
    ///
    /// A buffer's source knows no CRC, so the preliminary header is
    /// written again, with the time a callback set:
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('finish-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE);
    /// $z->addFromString('a', str_repeat('a', 40000)); $z->setCompressionName('a', ZipArchive::CM_STORE); $z->setMtimeName('a', 315532800);
    /// $z->registerProgressCallback(0.1, function ($r) use ($z) { static $d; if ($r > 0.1 && !$d) { $d = 1; $z->setMtimeName('a', 946684800); } });
    /// var_dump($z->close()); $b = file_get_contents('finish-witness.zip');
    /// echo bin2hex(substr($b, 10, 4)), ' ', bin2hex(substr($b, strpos($b, "PK\1\2") + 12, 4)), "\n"; unlink('finish-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// 00002128 00002128
    /// ```
    ///
    /// An unchanged entry stored anew knows its CRC and sizes before its
    /// data, so nothing settles otherwise and its local header keeps the
    /// time it was written with, while the central record has the one a
    /// callback set:
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('dirent-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE);
    /// $z->addFromString('a', str_repeat('a', 40000)); $z->setMtimeName('a', 315532800); $z->close();
    /// $z->open('dirent-witness.zip'); $z->setCompressionName('a', ZipArchive::CM_STORE);
    /// $z->registerProgressCallback(0.1, function ($r) use ($z) { static $d; if ($r > 0.1 && !$d) { $d = 1; $z->setMtimeName('a', 946684800); } });
    /// var_dump($z->close()); $b = file_get_contents('dirent-witness.zip');
    /// echo bin2hex(substr($b, 10, 4)), ' ', bin2hex(substr($b, strpos($b, "PK\1\2") + 12, 4)), "\n";
    /// $z->open('dirent-witness.zip'); var_dump(strlen($z->getFromName('a'))); $z->close(); unlink('dirent-witness.zip');
    /// ```
    /// ```expect capture
    /// bool(true)
    /// 00002100 00002128
    /// int(40000)
    /// ```
    fn finish(
        &mut self,
        i: usize,
        written: &Written,
        at: u64,
        out: &mut Out,
    ) -> Result<(), Status> {
        let settled = match &written.data {
            Data::Add(added) => {
                let settled = added.settled();
                if let Err(status) = data_end(&added.end, settled.crc, settled.size) {
                    self.error = status;
                    return Err(status);
                }
                Some(settled)
            }
            Data::Copy(_) => None,
        };
        let torrentzip = self.want_torrentzip();
        let Some(de) = self.entries.get_mut(i).and_then(|e| e.changes.as_mut()) else {
            self.error.set(Er::Internal);
            return Err(self.error);
        };
        if let Some(settled) = &settled {
            let force = settled.zip64.forced;
            let mut changed = settled.settle(de);
            if !torrentzip {
                // `dirent_changed`'s mtime half never holds: a source
                // states one mtime, a buffer's when it was made and a
                // file's when it was added, so the stat after the copy
                // gives the one before it.
                changed |= apply_attributes(de, &settled.attributes, force);
            }
            if changed {
                out.rewrite(at, &dirent_bytes(de, Kind::Local, torrentzip, force)?)?;
                // The header written again must fit where the preliminary
                // one stood, with a Zip64 field as it had one or not.
                if is_zip64(de, Kind::Local, force) != settled.zip64.written {
                    self.error.set(Er::Internal);
                    return Err(self.error);
                }
            }
        }
        if de.bitflags & GPBF_DATA_DESCRIPTOR != 0 {
            let zip64 = settled
                .as_ref()
                .map_or_else(|| needs_zip64(de, Kind::Local), |s| s.zip64.written);
            out.write(&data_descriptor(de, zip64))?;
        }
        Ok(())
    }

    /// `write_cdir`: the central directory, then the Zip64 end records
    /// where they are needed, then the end record with the comment, or
    /// torrentzip's in its place, `ZIP_WANT_TORRENTZIP` read past the last
    /// callback.
    fn write_cdir(
        &mut self,
        out: &mut Out,
        list: &[(usize, Option<Vec<u8>>)],
    ) -> Result<(), Status> {
        let torrentzip = self.want_torrentzip();
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
            out.write(&dirent_bytes(de, Kind::Central, torrentzip, false)?)?;
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

    /// `add_data`: the entry's source or its old self, under the layers
    /// that decrypt, decompress, compress and encrypt it as its dirent
    /// asks ([`Pipe`]), which the close's loop reads as it writes, the
    /// dirent written before it. An unchanged entry is read as
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
    fn add_data(&mut self, i: usize, de: &mut Dirent, torrentzip: bool) -> Result<Written, Status> {
        let Sourced { before, input, end } = self.source(i, de.changed & D_COMP_METHOD != 0)?;
        // The preliminary dirent, written before the data: what the stat
        // knows, a stored entry's compressed size its size, and what it
        // does not know as the dirent held it.
        de.bitflags &= !GPBF_DATA_DESCRIPTOR;
        if let Some(size) = before.size {
            if de.comp_method == CM_STORE {
                de.comp_size = size;
            }
            de.uncomp_size = size;
        }
        if let Some(crc) = before.crc {
            de.crc = crc;
        }
        if de.changed & D_LAST_MOD == 0 {
            de.last_mod = match before.time {
                Time::Dos(dos) => dos,
                Time::Mtime(mtime) => self.zone.u2d(mtime),
            };
        }
        // `needs_compress`: what is stored needs no decompression, and a
        // method other than store is a recompression (`needs_recompress`).
        let compressor = match actual(de.comp_method) {
            0 => None,
            code => {
                let Some(method) = Method::of(code) else {
                    self.error.set(Er::Compnotsupp);
                    return Err(self.error);
                };
                let can_store = is_default(de.comp_method);
                Some(
                    Compressor::new(method, de.compression_level, can_store)
                        .inspect_err(|s| self.error = *s)?,
                )
            }
        };
        // `ZIP_FL_FORCE_ZIP64` where the source states no size, or what
        // the entry compresses to could pass 32 bits, and so its local
        // header holds room for it (zip_close.c:364-396).
        let forced = before.size.is_none_or(|size| {
            compressor
                .as_ref()
                .is_some_and(|c| c.maximum_size(size) > u64::from(u32::MAX))
        });
        let sealer = if de.encryption_method == EM_NONE {
            None
        } else {
            let password = de
                .password
                .clone()
                .or_else(|| self.default_password.clone());
            let Some(password) = password else {
                self.error.set(Er::Inval);
                return Err(self.error);
            };
            Some(match Encryption::of(de.encryption_method) {
                Encryption::Pkware => {
                    de.bitflags |= GPBF_DATA_DESCRIPTOR;
                    Sealer::pkware(&password, de.last_mod.time)?
                }
                Encryption::Aes(strength) => Sealer::aes(&password, strength)?,
                Encryption::None | Encryption::Unknown => {
                    self.error.set(Er::Encrnotsupp);
                    return Err(self.error);
                }
            })
        };
        let pipe = Pipe::new(input, before.size, before.attributes, compressor, sealer);
        if !torrentzip {
            apply_attributes(de, &pipe.attributes(), forced);
        }
        let header = dirent_bytes(de, Kind::Local, torrentzip, forced)?;
        let zip64 = Zip64 {
            forced,
            written: is_zip64(de, Kind::Local, forced),
        };
        let added = Added { pipe, end, zip64 };
        Ok(Written {
            header,
            data: Data::Add(Box::new(added)),
        })
    }

    /// What `add_data` copies from entry `i`: a new source opened, or the
    /// unchanged entry through `zip_source_zip_file_create(za, i,
    /// ZIP_FL_UNCHANGED, ...)`, decrypted with the default password and
    /// decompressed, its method changed where `method_changed` says; the stat before the
    /// copy, which says either is stored and unencrypted, and how its end
    /// is held ([`data_end`]).
    fn source(&mut self, i: usize, method_changed: bool) -> Result<Sourced, Status> {
        if let Some(s) = &mut self.entries[i].source {
            let st = s.stat();
            let before = Before {
                size: s.size(),
                crc: None,
                time: Time::Mtime(st.mtime),
                attributes: Attributes {
                    host: s.attributes(),
                    ..Attributes::default()
                },
            };
            let input = s.open(i).inspect_err(|s| self.error = *s)?;
            return Ok(Sourced {
                end: End::Size(before.size),
                before,
                input,
            });
        }
        let password = self.default_password.clone();
        let input = self
            .unchanged_input(i, password.as_deref(), method_changed)
            .inspect_err(|s| self.error = *s)?;
        let Some(o) = self.entries[i].orig.as_ref() else {
            return Err(Status::new(Er::Internal));
        };
        let crc = o.crc_valid.then_some(o.crc);
        let end = End::Entry {
            stored: actual(o.comp_method) == 0,
            size: o.uncomp_size,
            crc,
        };
        let before = Before {
            size: Some(o.uncomp_size),
            crc,
            time: Time::Dos(o.last_mod),
            attributes: Attributes::unchanged(o),
        };
        Ok(Sourced { before, input, end })
    }
}

/// What `add_data` copies from entry `i`: its source's stat before the
/// copy, the source opened, and how its end is held.
struct Sourced {
    before: Before,
    input: Input,
    end: End,
}

/// What `add_data`'s `zip_source_stat` of its source says before the
/// copy: the size and the CRC where the source knows them, the time the
/// dirent takes where none was set, and the attributes the source gives.
struct Before {
    size: Option<u64>,
    crc: Option<u32>,
    time: Time,
    attributes: Attributes,
}

/// The time a source gives an entry: an unchanged entry's DOS time as it
/// stands, or a source's mtime, converted (`zip_source_get_dos_time`, or
/// `_zip_u2d_time` of `st.mtime`).
enum Time {
    Dos(DosTime),
    Mtime(i64),
}

/// What the end of the data `add_data` copies finds, the CRC and the
/// length `read` of what its source gave held as `end` says: for a new
/// source, `ZIP_ER_DATA_LENGTH` where it gave other than its stated
/// size, a buffer a stream left part read; for an unchanged entry,
/// `ZIP_ER_DATA_LENGTH` for stored data whose length is not the entry's
/// size, from the stat of its CRC layer
/// after the copy, which no decompressor below has given the size it
/// made (a decryption layer takes its header off the compressed size
/// alone); otherwise `ZIP_ER_CRC` from `zip_source_zip_file_create`'s
/// CRC layer at the end of the data, where the entry has a CRC to hold
/// it to. An encrypted entry whose method is changed is not copied at
/// all, whether it keeps its encryption or not: its source's decryption
/// layer is refused, `ZIP_ER_OPNOTSUPP`, before its data
/// (`Archive::decoder`).
///
/// ```php
/// function crafted($f, $method, $comp, $size, $crc) {
///     $local = "PK\3\4" . pack('vvvvvVVVvv', 20, 0, $method, 0, 0x21, $crc, strlen($comp), $size, 1, 0) . 'c' . $comp;
///     $cd = "PK\1\2" . pack('vvvvvvVVVvvvvvVV', 0x0314, 20, 0, $method, 0, 0x21, $crc, strlen($comp), $size, 1, 0, 0, 0, 0, 0x81a40000, 0) . 'c';
///     file_put_contents($f, $local . $cd . "PK\5\6" . pack('vvvvVVv', 0, 0, 1, 1, strlen($cd), strlen($local), 0));
/// }
/// $cases = [
///     'stored, its CRC wrong' => [0, 'hello', 5, 1234],
///     'deflated, its CRC wrong' => [8, gzdeflate('hello'), 5, 1234],
///     'stored, its size long' => [0, 'hello', 6, crc32('hello')],
///     'stored, its size short and its CRC wrong' => [0, 'hello', 4, 1234],
///     'deflated, its size long' => [8, gzdeflate('hello'), 6, crc32('hello')],
///     'deflated, its size long and its CRC wrong' => [8, gzdeflate('hello'), 6, 1234],
/// ];
/// foreach ($cases as $name => [$m, $comp, $size, $crc]) {
///     crafted('end-witness.zip', $m, $comp, $size, $crc); $z = new ZipArchive; $z->open('end-witness.zip');
///     $z->setCompressionName('c', $m ? ZipArchive::CM_STORE : ZipArchive::CM_DEFLATE); $r = @$z->close();
///     echo "$name: ", var_export($r, true), " $z->status ", $z->getStatusString(), "\n";
///     if ($r) { $z->open('end-witness.zip'); echo '  ', $z->statIndex(0)['size'], ' ', $z->getFromIndex(0), "\n"; $z->close(); }
/// }
/// foreach ([ZipArchive::EM_TRAD_PKWARE, ZipArchive::EM_AES_256] as $em) {
///     $z = new ZipArchive; $z->open('end-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE);
///     $z->addFromString('c', str_repeat('secret ', 10)); $z->setCompressionName('c', ZipArchive::CM_STORE); $z->setEncryptionName('c', $em, 'pw'); $z->close();
///     $z->open('end-witness.zip'); $z->setPassword('pw'); $z->setEncryptionName('c', ZipArchive::EM_NONE); $r = @$z->close();
///     echo "stored, encrypted $em, decrypted: ", var_export($r, true), " $z->status ", $z->getStatusString(), "\n";
///     $z->open('end-witness.zip'); $s = $z->statIndex(0); echo "  {$s['encryption_method']} {$s['size']} {$s['comp_size']} ", strlen($z->getFromIndex(0)), "\n"; $z->close();
///     foreach ([ZipArchive::CM_DEFLATE => 'kept', ZipArchive::CM_BZIP2 => 'decrypted'] as $m => $then) {
///         $z->open('end-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE);
///         $z->addFromString('c', str_repeat('secret ', 10)); $z->setCompressionName('c', ZipArchive::CM_STORE); $z->setEncryptionName('c', $em, 'pw'); $z->close();
///         $z->open('end-witness.zip'); $z->setPassword('pw'); $z->setCompressionName('c', $m);
///         if ($then == 'decrypted') $z->setEncryptionName('c', ZipArchive::EM_NONE);
///         $r = @$z->close(); echo "stored, encrypted $em, method $m, encryption $then: ", var_export($r, true), " $z->status ", $z->getStatusString(), "\n";
///     }
/// }
/// $z = new ZipArchive; $z->open('end-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE);
/// $z->addFromString('c', 'tiny'); $z->setCompressionName('c', ZipArchive::CM_STORE); $z->setEncryptionName('c', ZipArchive::EM_AES_256, 'pw'); $z->close();
/// $z->open('end-witness.zip'); $z->setPassword('pw'); $z->setEncryptionName('c', ZipArchive::EM_NONE); $r = @$z->close();
/// echo "stored, AE-2 with no CRC, decrypted: ", var_export($r, true), " $z->status ", $z->getStatusString(), "\n";
/// $z->open('end-witness.zip'); $s = $z->statIndex(0); echo "  {$s['crc']} {$s['size']} ", $z->getFromIndex(0), "\n"; $z->close();
/// unlink('end-witness.zip');
/// ```
/// ```expect capture
/// stored, its CRC wrong: false 7 CRC error
/// deflated, its CRC wrong: false 7 CRC error
/// stored, its size long: false 33 Unexpected length of data
/// stored, its size short and its CRC wrong: false 33 Unexpected length of data
/// deflated, its size long: true 0 No error
///   5 hello
/// deflated, its size long and its CRC wrong: false 7 CRC error
/// stored, encrypted 1, decrypted: true 0 No error
///   0 70 70 70
/// stored, encrypted 1, method 8, encryption kept: false 28 Operation not supported
/// stored, encrypted 1, method 12, encryption decrypted: false 28 Operation not supported
/// stored, encrypted 259, decrypted: true 0 No error
///   0 70 70 70
/// stored, encrypted 259, method 8, encryption kept: false 28 Operation not supported
/// stored, encrypted 259, method 12, encryption decrypted: false 28 Operation not supported
/// stored, AE-2 with no CRC, decrypted: true 0 No error
///   1354274761 4 tiny
/// ```
fn data_end(end: &End, crc: u32, read: u64) -> Result<(), Status> {
    match *end {
        End::Size(Some(size))
        | End::Entry {
            stored: true, size, ..
        } if read != size => Err(Status::new(Er::DataLength)),
        End::Entry {
            crc: Some(expect), ..
        } if crc != expect => Err(Status::new(Er::Crc)),
        End::Size(_) | End::Entry { .. } => Ok(()),
    }
}

/// How the end of the data `add_data` copies is held, past the copy.
enum End {
    /// A new source, by its CRC layer's stat: what it read must be the
    /// size it stated, where it stated one.
    Size(Option<u64>),
    /// An unchanged entry, by its original dirent: stored data's length
    /// its size, then its CRC where it has one.
    Entry {
        stored: bool,
        size: u64,
        crc: Option<u32>,
    },
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
