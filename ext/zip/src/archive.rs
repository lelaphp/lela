//! libzip's archive re-derived (`zipint.h`'s `zip_t`, `zip_entry_t` and
//! `zip_dirent_t`): the entries as the file had them and as the program
//! changed them, the changes held until `close` writes them, each call
//! failing with libzip's code where libzip's fails. The central directory
//! is read by [`crate::cdir`], every entry's data by [`crate::read`].

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use crate::cdir::{self, Local};
use crate::dostime::{DosTime, TORRENTZIP, Zone};
use crate::error::{Done, Er, Failed, Status};
use crate::read::{Layers, Reader, Stdio, Window};
use crate::strings::{self, Enc, FL_ENC_RAW, FL_ENC_STRICT, FL_ENC_UTF_8, ZStr};

// `zip_open` flags.
pub(crate) const CREATE: u32 = 1;
pub(crate) const EXCL: u32 = 2;
pub(crate) const CHECKCONS: u32 = 4;
pub(crate) const TRUNCATE: u32 = 8;
pub(crate) const RDONLY: u32 = 16;
// `zip_flags_t`.
pub(crate) const FL_NOCASE: u32 = 1;
pub(crate) const FL_NODIR: u32 = 2;
pub(crate) const FL_COMPRESSED: u32 = 4;
pub(crate) const FL_UNCHANGED: u32 = 8;
pub(crate) const FL_ENCRYPTED: u32 = 32;
pub(crate) const FL_OVERWRITE: u32 = 8192;
// Archive flags.
pub(crate) const AFL_RDONLY: u32 = 2;
pub(crate) const AFL_IS_TORRENTZIP: u32 = 4;
pub(crate) const AFL_WANT_TORRENTZIP: u32 = 8;
pub(crate) const AFL_CREATE_OR_KEEP_FILE_FOR_EMPTY_ARCHIVE: u32 = 16;
// Compression methods.
pub(crate) const CM_DEFAULT: i32 = -1;
pub(crate) const CM_DEFLATE: i32 = 8;
pub(crate) const CM_BZIP2: i32 = 12;
pub(crate) const CM_LZMA: i32 = 14;
// Encryption methods.
pub(crate) const EM_NONE: u16 = 0;
pub(crate) const EM_TRAD_PKWARE: u16 = 1;
pub(crate) const EM_AES_128: u16 = 0x0101;
pub(crate) const EM_AES_192: u16 = 0x0102;
pub(crate) const EM_AES_256: u16 = 0x0103;
pub(crate) const EM_UNKNOWN: u16 = 0xffff;

/// What an encryption method does to an entry's data, the one place its
/// number is read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encryption {
    None,
    /// PKWARE's traditional encryption, a 12-byte header before the data.
    Pkware,
    /// `WinZip` AES at a strength of 1, 2 or 3, for 128, 192 and 256 bits.
    Aes(u8),
    /// A method libzip does not know.
    Unknown,
}

impl Encryption {
    pub(crate) fn of(method: u16) -> Encryption {
        match method {
            EM_NONE => Encryption::None,
            EM_TRAD_PKWARE => Encryption::Pkware,
            EM_AES_128 => Encryption::Aes(1),
            EM_AES_192 => Encryption::Aes(2),
            EM_AES_256 => Encryption::Aes(3),
            _ => Encryption::Unknown,
        }
    }

    /// The bytes the method adds to the data: PKWARE's header, or
    /// `WinZip` AES's salt of `4 * strength + 4` bytes, its 2-byte
    /// password check and 10-byte code; `None` for a method not known.
    pub(crate) fn overhead(self) -> Option<u64> {
        match self {
            Encryption::None => Some(0),
            Encryption::Pkware => Some(12),
            Encryption::Aes(strength) => Some(4 * u64::from(strength) + 16),
            Encryption::Unknown => None,
        }
    }

    /// The method's number, `ZIP_EM_*`.
    pub(crate) fn method(self) -> Option<u16> {
        match self {
            Encryption::None => Some(EM_NONE),
            Encryption::Pkware => Some(EM_TRAD_PKWARE),
            Encryption::Aes(1) => Some(EM_AES_128),
            Encryption::Aes(2) => Some(EM_AES_192),
            Encryption::Aes(3) => Some(EM_AES_256),
            Encryption::Aes(_) | Encryption::Unknown => None,
        }
    }

    /// The `WinZip` AES strength, for an AES method.
    pub(crate) fn aes(self) -> Option<u8> {
        match self {
            Encryption::Aes(strength) => Some(strength),
            Encryption::None | Encryption::Pkware | Encryption::Unknown => None,
        }
    }
}

// `ZIP_DIRENT_*`: what a changed entry changed.
pub(crate) const D_COMP_METHOD: u32 = 0x01;
pub(crate) const D_FILENAME: u32 = 0x02;
pub(crate) const D_COMMENT: u32 = 0x04;
pub(crate) const D_EXTRA_FIELD: u32 = 0x08;
pub(crate) const D_ATTRIBUTES: u32 = 0x10;
pub(crate) const D_LAST_MOD: u32 = 0x20;
pub(crate) const D_ENCRYPTION_METHOD: u32 = 0x40;
pub(crate) const D_PASSWORD: u32 = 0x80;
/// `ZIP_OPSYS_DEFAULT`, `ZIP_OPSYS_UNIX`.
pub(crate) const OPSYS_DEFAULT: u8 = 3;
/// `ZIP_EXT_ATTRIB_DEFAULT`.
pub(crate) const EXT_ATTRIB_DEFAULT: u32 = 0o100_666 << 16;
/// `ZIP_EXT_ATTRIB_DEFAULT_DIR`.
pub(crate) const EXT_ATTRIB_DEFAULT_DIR: u32 = 0o40_777 << 16;
/// The general purpose bits: encrypted, `PK\x07\x08` record after the data, strong
/// encryption and UTF-8.
pub(crate) const GPBF_ENCRYPTED: u16 = 0x0001;
pub(crate) const GPBF_DATA_DESCRIPTOR: u16 = 0x0008;
pub(crate) const GPBF_STRONG_ENCRYPTION: u16 = 0x0040;
pub(crate) const GPBF_ENCODING_UTF_8: u16 = 0x0800;

/// `method == ZIP_CM_DEFAULT`: libzip 1.12 dropped
/// `ZIP_CM_REPLACED_DEFAULT`, so the one default is the method left to
/// libzip.
pub(crate) fn is_default(method: i32) -> bool {
    method == CM_DEFAULT
}

/// `ZIP_CM_ACTUAL`.
pub(crate) fn actual(method: i32) -> u16 {
    if is_default(method) {
        8
    } else {
        u16::try_from(method & 0xffff).unwrap_or(0)
    }
}

/// `zip_compression_method_supported`: the methods this build writes
/// and reads, the oracle's libzip's set.
pub(crate) fn compression_supported(method: i64) -> bool {
    matches!(method, -1 | 0 | 8 | 12 | 14 | 93 | 95)
}

/// `zip_encryption_method_supported`.
pub(crate) fn encryption_supported(method: i64) -> bool {
    matches!(method, 0 | 1 | 0x0101..=0x0103)
}

/// `zip_dirent_t`: one central directory record, as read or as changed.
#[derive(Clone, Debug)]
pub(crate) struct Dirent {
    pub(crate) changed: u32,
    pub(crate) crc_valid: bool,
    pub(crate) version_madeby: u16,
    pub(crate) version_needed: u16,
    pub(crate) bitflags: u16,
    pub(crate) comp_method: i32,
    pub(crate) last_mod: DosTime,
    /// `last_mod_mtime`, where `zip_file_set_mtime` gave it.
    pub(crate) mtime: Option<i64>,
    pub(crate) crc: u32,
    pub(crate) comp_size: u64,
    pub(crate) uncomp_size: u64,
    pub(crate) filename: Option<ZStr>,
    /// The extra fields of the local header, libzip's own taken out;
    /// `None` until read.
    pub(crate) extra_local: Option<Vec<u8>>,
    pub(crate) extra_central: Vec<u8>,
    pub(crate) comment: Option<ZStr>,
    pub(crate) disk_number: u32,
    pub(crate) int_attrib: u16,
    pub(crate) ext_attrib: u32,
    pub(crate) offset: u64,
    pub(crate) compression_level: u32,
    pub(crate) encryption_method: u16,
    pub(crate) password: Option<Vec<u8>>,
}

impl Default for Dirent {
    /// `_zip_dirent_init`.
    fn default() -> Dirent {
        Dirent {
            changed: 0,
            crc_valid: true,
            version_madeby: 63 | (u16::from(OPSYS_DEFAULT) << 8),
            version_needed: 10,
            bitflags: 0,
            comp_method: CM_DEFAULT,
            last_mod: DosTime::default(),
            mtime: None,
            crc: 0,
            comp_size: 0,
            uncomp_size: 0,
            filename: None,
            extra_local: Some(Vec::new()),
            extra_central: Vec::new(),
            comment: None,
            disk_number: 0,
            int_attrib: 0,
            ext_attrib: EXT_ATTRIB_DEFAULT,
            offset: 0,
            compression_level: 0,
            encryption_method: EM_NONE,
            password: None,
        }
    }
}

/// `_zip_dirent_clone`: a copy with nothing marked changed.
fn clone_of(orig: Option<&Dirent>) -> Dirent {
    let mut de = orig.cloned().unwrap_or_default();
    de.changed = 0;
    de
}

/// What new data an entry is given, `zip_source_t` for the sources php
/// makes: a string, or a file read at `close`.
#[derive(Debug)]
pub(crate) enum Source {
    Buffer {
        data: Vec<u8>,
        mtime: i64,
    },
    File {
        path: PathBuf,
        /// The file `ZipArchive::FL_OPEN_FILE_NOW` opened.
        file: Option<File>,
        start: u64,
        len: u64,
        mtime: i64,
        mode: u32,
    },
}

impl Source {
    /// `zip_source_buffer`, made now.
    pub(crate) fn buffer(data: Vec<u8>) -> Source {
        Source::Buffer { data, mtime: now() }
    }

    /// `zip_source_file` and `zip_source_filep`: the file stated now, the
    /// window held to its size; `None` for a file that is gone, which
    /// libzip refuses without an error.
    pub(crate) fn file(
        path: PathBuf,
        file: Option<File>,
        start: u64,
        len: i64,
    ) -> Result<Option<Source>, Er> {
        use std::os::unix::fs::MetadataExt;
        let length = u64::try_from(len).unwrap_or(0);
        if start > i64::MAX as u64 || start.checked_add(length).is_none() {
            return Err(Er::Inval);
        }
        let meta = match &file {
            Some(f) => f.metadata(),
            None => std::fs::metadata(&path),
        };
        let Ok(meta) = meta else {
            return Ok(None);
        };
        let mut length = length;
        if meta.is_file() {
            if start + length > meta.len() {
                return Err(Er::Inval);
            }
            if length == 0 {
                length = meta.len() - start;
            }
        }
        Ok(Some(Source::File {
            path,
            file,
            start,
            len: length,
            mtime: meta.mtime(),
            mode: meta.mode(),
        }))
    }

    /// The source's `ZIP_SOURCE_STAT`.
    pub(crate) fn stat(&self) -> Stat {
        match self {
            Source::Buffer { data, mtime } => Stat {
                size: data.len() as u64,
                comp_size: data.len() as u64,
                mtime: *mtime,
                ..Stat::default()
            },
            Source::File { len, mtime, .. } => Stat {
                size: *len,
                mtime: *mtime,
                ..Stat::default()
            },
        }
    }

    /// `ZIP_SOURCE_GET_FILE_ATTRIBUTES`: a file's host and permissions, the
    /// read-only bit where the owner may not write.
    pub(crate) fn attributes(&self) -> Option<(u8, u32)> {
        match self {
            Source::Buffer { .. } => None,
            Source::File { mode, .. } => Some((3, (mode << 16) | u32::from(mode & 0o200 == 0))),
        }
    }

    /// The source opened for reading (`ZIP_SOURCE_OPEN`): a string's
    /// bytes, or a window on the file read as it is read.
    pub(crate) fn reader(&self) -> Result<Reader, Status> {
        match self {
            Source::Buffer { data, .. } => Ok(Reader::bytes(data.clone())),
            Source::File {
                path,
                file,
                start,
                len,
                ..
            } => {
                let f = match file {
                    Some(f) => f.try_clone(),
                    None => File::open(path),
                }
                .map_err(|e| Status::os(Er::Open, &e))?;
                let f = Stdio::new(f);
                Ok(Reader::window(Window::new(f, *start, *len, false), None))
            }
        }
    }

    /// Every byte of the source.
    pub(crate) fn read_all(&mut self) -> Result<Vec<u8>, Status> {
        match self {
            Source::Buffer { data, .. } => Ok(data.clone()),
            Source::File {
                path,
                file,
                start,
                len,
                ..
            } => {
                let mut opened;
                let f = if let Some(f) = file {
                    f
                } else {
                    opened = File::open(&*path).map_err(|e| Status::os(Er::Open, &e))?;
                    &mut opened
                };
                f.seek(SeekFrom::Start(*start))
                    .map_err(|e| Status::os(Er::Seek, &e))?;
                let mut out = Vec::new();
                f.take(*len)
                    .read_to_end(&mut out)
                    .map_err(|e| Status::os(Er::Read, &e))?;
                if (out.len() as u64) < *len {
                    return Err(Status::new(Er::DataLength));
                }
                Ok(out)
            }
        }
    }
}

/// `time(NULL)`.
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `zip_entry_t`.
#[derive(Debug, Default)]
pub(crate) struct Entry {
    pub(crate) orig: Option<Dirent>,
    pub(crate) changes: Option<Dirent>,
    pub(crate) source: Option<Source>,
    pub(crate) deleted: bool,
}

impl Entry {
    /// `ZIP_ENTRY_CHANGED`.
    pub(crate) fn changed(&self, what: u32) -> bool {
        self.changes.as_ref().is_some_and(|c| c.changed & what != 0)
    }

    /// `ZIP_ENTRY_HAS_CHANGES`.
    pub(crate) fn has_changes(&self) -> bool {
        self.source.is_some() || self.deleted || self.changed(u32::MAX)
    }
}

/// `zip_stat_t`, its fields as php's `RETURN_SB` prints them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Stat {
    pub(crate) name: Vec<u8>,
    pub(crate) index: u64,
    pub(crate) crc: u32,
    pub(crate) size: u64,
    pub(crate) mtime: i64,
    pub(crate) comp_size: u64,
    pub(crate) comp_method: u16,
    pub(crate) encryption_method: u16,
}

/// The file the archive was read from, and its permissions.
pub(crate) struct Data {
    pub(crate) file: File,
    pub(crate) mode: u32,
    /// Its size at the open, which libzip's file source stats once.
    pub(crate) size: u64,
    /// The stdio `FILE` libzip reads it through, lent to the archive while
    /// the archive's object lends it out to its entries' streams.
    pub(crate) stdio: Option<Stdio>,
}

impl Data {
    /// `op` on that `FILE`, or on a fresh one while it is lent out.
    pub(crate) fn through<T>(&mut self, op: impl FnOnce(&mut Stdio) -> T) -> Option<T> {
        match &mut self.stdio {
            Some(f) => Some(op(f)),
            None => self.file.try_clone().ok().map(|f| op(&mut Stdio::new(f))),
        }
    }
}

/// `zip_t`.
pub(crate) struct Archive {
    pub(crate) path: PathBuf,
    pub(crate) open_flags: u32,
    pub(crate) error: Status,
    pub(crate) flags: u32,
    pub(crate) ch_flags: u32,
    pub(crate) default_password: Option<Vec<u8>>,
    pub(crate) comment_orig: Option<ZStr>,
    pub(crate) comment_changes: Option<ZStr>,
    pub(crate) comment_changed: bool,
    pub(crate) entries: Vec<Entry>,
    pub(crate) data: Option<Data>,
    pub(crate) zone: Zone,
    pub(crate) progress: Option<f64>,
    pub(crate) cancel: bool,
}

impl Archive {
    /// `_zip_allocate_new`.
    fn new(path: PathBuf, flags: u32) -> Archive {
        let rdonly = if flags & RDONLY != 0 { AFL_RDONLY } else { 0 };
        Archive {
            path,
            open_flags: flags,
            error: Status::default(),
            flags: rdonly,
            ch_flags: rdonly,
            default_password: None,
            comment_orig: None,
            comment_changes: None,
            comment_changed: false,
            entries: Vec::new(),
            data: None,
            zone: Zone::local(),
            progress: None,
            cancel: false,
        }
    }

    /// `zip_open`: the archive at `path` under `flags`, or the code and
    /// `errno` it failed with.
    pub(crate) fn open(path: PathBuf, flags: u32) -> Result<Archive, Status> {
        use std::os::unix::fs::PermissionsExt;
        if flags & (RDONLY | TRUNCATE) == RDONLY | TRUNCATE {
            return Err(Status::new(Er::Rdonly));
        }
        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if flags & CREATE == 0 {
                    return Err(Status::new(Er::Noent));
                }
                return Ok(Archive::new(path, flags));
            }
            Err(e) => return Err(Status::os(Er::Read, &e)),
        };
        if !meta.is_file() {
            return Err(Status::new(Er::Opnotsupp));
        }
        if flags & EXCL != 0 {
            return Err(Status::new(Er::Exists));
        }
        let file = File::open(&path).map_err(|e| Status::os(Er::Open, &e))?;
        let mut za = Archive::new(path, flags);
        if flags & TRUNCATE != 0 {
            return Ok(za);
        }
        za.read_cdir(file, meta.len(), meta.permissions().mode())?;
        Ok(za)
    }

    /// `_zip_open` past the stat: the central directory found and read as
    /// `_zip_find_central_dir` reads it ([`cdir`]). A file with no end
    /// record is `ER_TRUNCATED_ZIP` when its first four bytes are a local
    /// header, read from offset 0 as `_is_truncated_zip` seeks there:
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('nocd-witness.zip', ZipArchive::CREATE|ZipArchive::OVERWRITE);
    /// $z->addFromString('a', str_repeat('a', 3000)); $z->setCompressionName('a', ZipArchive::CM_STORE); $z->close();
    /// $h = fopen('nocd-witness.zip', 'r+'); ftruncate($h, 1000); fclose($h);
    /// var_dump($z->open('nocd-witness.zip')); unlink('nocd-witness.zip');
    /// ```
    /// ```expect
    /// int(35)
    /// ```
    fn read_cdir(&mut self, mut file: File, len: u64, mode: u32) -> Result<(), Status> {
        let cd = cdir::find(&mut file, len, self.open_flags).map_err(|e| {
            let mut head = [0u8; 4];
            let truncated = e.er == Er::Nozip
                && file.seek(SeekFrom::Start(0)).is_ok()
                && file.read_exact(&mut head).is_ok()
                && head == *b"PK\x03\x04";
            if truncated {
                Status::new(Er::TruncatedZip)
            } else {
                e
            }
        })?;
        if is_torrentzip(&cd.comment, &cd.bytes) {
            self.flags |= AFL_IS_TORRENTZIP;
        } else if let Ok(c) = ZStr::new(&cd.comment, 0) {
            self.comment_orig = c;
        }
        self.entries = cd
            .entries
            .into_iter()
            .map(|de| Entry {
                orig: Some(de),
                ..Entry::default()
            })
            .collect();
        self.ch_flags = self.flags;
        if self.open_flags & CHECKCONS != 0 {
            self.check_names()?;
        }
        let stdio = file
            .try_clone()
            .map(|f| self.replay(Stdio::new(f), len, cd.offset, &cd.bytes))
            .ok();
        self.data = Some(Data {
            file,
            mode,
            size: len,
            stdio,
        });
        Ok(())
    }

    /// The reads `zip_open` makes through the archive's `FILE`, so its
    /// buffer holds what php's does: the end of the file
    /// (`_zip_find_central_dir`), the central directory where that did not
    /// reach it, and under `ZIP_CHECKCONS` each local header.
    ///
    /// What the buffer held at the open is read after the file is cut:
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('cut-witness.zip', ZipArchive::CREATE|ZipArchive::OVERWRITE);
    /// $z->addFromString('a', str_repeat('a', 1000)); $z->setCompressionName('a', ZipArchive::CM_STORE); $z->close();
    /// $z->open('cut-witness.zip'); $h = fopen('cut-witness.zip', 'r+'); ftruncate($h, 10); fclose($h);
    /// var_dump(strlen($z->getFromName('a')), $z->getStatusString()); $z->close(); unlink('cut-witness.zip');
    /// ```
    /// ```expect
    /// int(1000)
    /// string(8) "No error"
    /// ```
    fn replay(&self, mut f: Stdio, len: u64, start: u64, cdir: &[u8]) -> Stdio {
        let tail = len.min(cdir::CDBUFSIZE);
        f.seek(len - tail);
        f.read(&mut vec![0; usize::try_from(tail).unwrap_or(0)])
            .ok();
        if start < len - tail {
            f.seek(start);
            let mut at = 0;
            while let Some(lens) = cdir.get(at + 28..at + 34) {
                let mut piece = vec![0; 46];
                f.read(&mut piece).ok();
                for k in 0..3 {
                    piece.resize(
                        usize::from(u16::from_le_bytes([lens[2 * k], lens[2 * k + 1]])),
                        0,
                    );
                    f.read(&mut piece).ok();
                    at += piece.len();
                }
                at += 46;
            }
        }
        if self.open_flags & CHECKCONS != 0 {
            for e in self.entries.iter().filter_map(|e| e.orig.as_ref()) {
                f.seek(e.offset);
                let mut head = [0u8; 30];
                if f.read(&mut head).is_ok_and(|n| n == 30) {
                    let n = u16::from_le_bytes([head[26], head[27]]);
                    let m = u16::from_le_bytes([head[28], head[29]]);
                    f.read(&mut vec![0; usize::from(n) + usize::from(m)]).ok();
                }
            }
        }
        f
    }

    /// `_zip_hash_add` under `ZIP_CHECKCONS`: a name twice is
    /// `ZIP_ER_EXISTS`.
    fn check_names(&self) -> Result<(), Status> {
        let mut seen = std::collections::HashSet::new();
        for e in &self.entries {
            let name =
                strings::get(e.orig.as_ref().and_then(|d| d.filename.as_ref()), 0).into_owned();
            if !seen.insert(name) {
                return Err(Status::new(Er::Exists));
            }
        }
        Ok(())
    }

    /// `zip_get_num_entries(za, 0)`.
    pub(crate) fn num_entries(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn fail<T>(&mut self, er: Er) -> Done<T> {
        self.error.set(er);
        Err(Failed)
    }

    /// `ZIP_IS_RDONLY`.
    fn rdonly(&self) -> bool {
        self.ch_flags & AFL_RDONLY != 0
    }

    /// `ZIP_WANT_TORRENTZIP`.
    pub(crate) fn want_torrentzip(&self) -> bool {
        self.ch_flags & AFL_WANT_TORRENTZIP != 0
    }

    /// The checks every setter makes first: read-only, then torrentzip.
    fn writable(&mut self, torrentzip_matters: bool) -> Done {
        if self.rdonly() {
            return self.fail(Er::Rdonly);
        }
        if torrentzip_matters && self.want_torrentzip() {
            return self.fail(Er::NotAllowed);
        }
        Ok(())
    }

    /// `_zip_get_dirent`, its error the code it would set.
    pub(crate) fn dirent(&self, idx: usize, flags: u32) -> Result<&Dirent, Er> {
        let e = self.entries.get(idx).ok_or(Er::Inval)?;
        match &e.changes {
            Some(c) if flags & FL_UNCHANGED == 0 => Ok(c),
            _ => {
                let orig = e.orig.as_ref().ok_or(Er::Inval)?;
                if e.deleted && flags & FL_UNCHANGED == 0 {
                    return Err(Er::Deleted);
                }
                Ok(orig)
            }
        }
    }

    /// `_zip_get_name` without setting an error.
    fn name_of(&self, idx: usize, flags: u32) -> Result<Vec<u8>, Er> {
        let de = self.dirent(idx, flags)?;
        Ok(strings::get(de.filename.as_ref(), flags).into_owned())
    }

    /// `zip_get_name`.
    pub(crate) fn get_name(&mut self, idx: usize, flags: u32) -> Done<Vec<u8>> {
        match self.name_of(idx, flags) {
            Ok(name) => Ok(name),
            Err(er) => self.fail(er),
        }
    }

    /// `_zip_name_locate`, its error the code it would set; each name
    /// compared as C reads it, a NUL it holds ending it, which libzip 1.12
    /// keeps and `ZIP_CHECKCONS` refuses at the open:
    ///
    /// ```php
    /// $l = "PK\3\4" . pack('vvvvvVVVvv', 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 3, 0) . "a\0b" . 'hello';
    /// $c = "PK\1\2" . pack('vvvvvvVVVvvvvvVV', 0x0314, 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 3, 0, 0, 0, 0, 0x81a40000, 0) . "a\0b";
    /// file_put_contents('nul-witness.zip', $l . $c . "PK\5\6" . pack('vvvvVVv', 0, 0, 1, 1, strlen($c), strlen($l), 0));
    /// $z = new ZipArchive; var_dump($z->open('nul-witness.zip', ZipArchive::CHECKCONS), $z->open('nul-witness.zip'));
    /// var_dump($z->getNameIndex(0), $z->statIndex(0)['name'], $z->locateName('a'), $z->getFromName('a'));
    /// $z->close(); unlink('nul-witness.zip');
    /// ```
    /// ```expect
    /// int(21)
    /// bool(true)
    /// string(1) "a"
    /// string(1) "a"
    /// int(0)
    /// string(5) "hello"
    /// ```
    fn locate(&self, fname: &[u8], flags: u32) -> Result<usize, Er> {
        if fname.len() > 0xffff {
            return Err(Er::Inval);
        }
        let converted;
        let fname = if flags & (FL_ENC_UTF_8 | FL_ENC_RAW) == 0 && !fname.is_empty() {
            let s = ZStr::new(fname, flags).map_err(|()| Er::Inval)?;
            converted = strings::get(s.as_ref(), 0).into_owned();
            converted.as_slice()
        } else {
            fname
        };
        let linear = flags & (FL_NOCASE | FL_NODIR | FL_ENC_RAW | FL_ENC_STRICT) != 0;
        for i in 0..self.entries.len() {
            let name_flags = if linear { flags } else { flags & FL_UNCHANGED };
            let Ok(name) = self.name_of(i, name_flags) else {
                continue;
            };
            // `strcmp` on the name as C reads it: libzip 1.12 keeps a NUL
            // a name holds, where 1.11.4 made it a space.
            let name = php_ext::coerce::c_str(&name).to_vec();
            if !linear && flags & FL_UNCHANGED == 0 && self.entries[i].deleted {
                continue;
            }
            let name = if flags & FL_NODIR != 0 {
                match name.iter().rposition(|b| *b == b'/') {
                    Some(at) => name[at + 1..].to_vec(),
                    None => name,
                }
            } else {
                name
            };
            let same = if flags & FL_NOCASE != 0 {
                name.eq_ignore_ascii_case(fname)
            } else {
                name == fname
            };
            if same {
                return Ok(i);
            }
        }
        Err(Er::Noent)
    }

    /// `zip_name_locate`, which sets `ZIP_ER_NOENT` where nothing matches.
    pub(crate) fn name_locate(&mut self, fname: &[u8], flags: u32) -> Done<usize> {
        match self.locate(fname, flags) {
            Ok(i) => Ok(i),
            Err(er) => self.fail(er),
        }
    }

    /// `zip_stat_index`.
    pub(crate) fn stat_index(&mut self, idx: usize, flags: u32) -> Done<Stat> {
        let Ok(de) = self.dirent(idx, flags) else {
            let er = self.dirent(idx, flags).err().unwrap_or(Er::Inval);
            return self.fail(er);
        };
        let de = de.clone();
        let name = self.get_name(idx, flags)?;
        let entry = &self.entries[idx];
        let mut st = if flags & FL_UNCHANGED == 0
            && let Some(source) = &entry.source
        {
            let mut st = source.stat();
            if !is_default(de.comp_method) {
                st.comp_method = u16::try_from(de.comp_method & 0xffff).unwrap_or(0);
                if st.comp_method == 0 {
                    st.comp_size = st.size;
                }
            }
            if entry.changed(D_LAST_MOD) {
                st.mtime = self.mtime_of(&de);
            }
            st
        } else {
            Stat {
                crc: de.crc,
                size: de.uncomp_size,
                mtime: self.mtime_of(&de),
                comp_size: de.comp_size,
                comp_method: u16::try_from(de.comp_method & 0xffff).unwrap_or(0),
                encryption_method: de.encryption_method,
                ..Stat::default()
            }
        };
        if self.want_torrentzip() && flags & FL_UNCHANGED == 0 {
            st.comp_method = 8;
            st.mtime = self.zone.d2u(TORRENTZIP);
        }
        st.index = idx as u64;
        st.name = name;
        Ok(st)
    }

    /// `zip_dirent_get_last_mod_mtime`.
    fn mtime_of(&self, de: &Dirent) -> i64 {
        de.mtime.unwrap_or_else(|| self.zone.d2u(de.last_mod))
    }

    /// `_zip_set_name`: a name past 65535 bytes refused, as libzip 1.12
    /// refuses it where 1.11.4 cut its length to 16 bits.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('name-len-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE);
    /// var_dump($z->addFromString(str_repeat('n', 65536), 'x'), $z->getStatusString(), $z->numFiles);
    /// var_dump($z->addFromString(str_repeat('n', 65535), 'x'), $z->numFiles, $z->renameIndex(0, str_repeat('r', 65536)), strlen($z->getNameIndex(0)));
    /// $z->close(); unlink('name-len-witness.zip');
    /// ```
    /// ```expect
    /// bool(false)
    /// string(16) "Invalid argument"
    /// int(0)
    /// bool(true)
    /// int(1)
    /// bool(false)
    /// int(65535)
    /// ```
    fn set_name(&mut self, idx: usize, name: &[u8], flags: u32) -> Done {
        if idx >= self.entries.len() {
            return self.fail(Er::Inval);
        }
        if self.rdonly() {
            return self.fail(Er::Rdonly);
        }
        let s = if name.is_empty() {
            None
        } else if name.len() > 0xffff {
            return self.fail(Er::Inval);
        } else {
            match ZStr::new(name, flags) {
                Ok(s) => s.map(|s| {
                    if flags & (FL_ENC_UTF_8 | strings::FL_ENC_CP437) == 0 {
                        s.promoted()
                    } else {
                        s
                    }
                }),
                Err(()) => return self.fail(Er::Inval),
            }
        };
        match self.locate(name, 0) {
            Ok(i) if i != idx => return self.fail(Er::Exists),
            Ok(_) => return Ok(()),
            Err(_) => {}
        }
        let e = &mut self.entries[idx];
        let same_as_orig = e
            .orig
            .as_ref()
            .is_some_and(|o| o.filename.as_ref().map(|f| &f.raw) == s.as_ref().map(|f| &f.raw));
        if same_as_orig {
            if let Some(c) = &mut e.changes
                && c.changed & D_FILENAME != 0
            {
                c.changed &= !D_FILENAME;
                if c.changed == 0 {
                    e.changes = None;
                } else {
                    c.filename = e.orig.as_ref().and_then(|o| o.filename.clone());
                }
            }
        } else {
            let c = e.changes.get_or_insert_with(|| clone_of(e.orig.as_ref()));
            c.changed |= D_FILENAME;
            c.filename = s;
        }
        Ok(())
    }

    /// `_zip_file_replace`: `idx` given new data, or with `None` a new
    /// entry, or the one named `name` under `ZIP_FL_OVERWRITE`. Its extra
    /// fields are cleared while torrentzip is wanted too, as 1.11.4 and
    /// 1.12.1 clear them and 1.12 alone refuses (#2667). Its method is
    /// the entry's until the close, whose merge leaves new data to the
    /// default method: libzip 1.12 marks no `ZIP_CM_REPLACED_DEFAULT`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('rp.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE);
    /// $z->addFromString('s', str_repeat('s', 999)); $z->setCompressionName('s', ZipArchive::CM_STORE);
    /// $z->addFromString('d', str_repeat('d', 999)); $z->addFromString('t', str_repeat('t', 999)); $z->close();
    /// $z->open('rp.zip');
    /// $z->addFromString('s', str_repeat('S', 999)); $z->addFromString('d', 'x');
    /// $z->addFromString('t', str_repeat('T', 999)); $z->unchangeName('t');
    /// foreach (['s', 'd', 't'] as $n) echo $n, ' ', $z->statName($n)['comp_method'], "\n";
    /// $z->close(); $z->open('rp.zip');
    /// foreach (['s', 'd', 't'] as $n) { $st = $z->statName($n); echo $n, ' ', $st['comp_method'], ' ', $st['comp_size'], "\n"; }
    /// $z->close(); unlink('rp.zip');
    /// ```
    /// ```expect
    /// s 0
    /// d 8
    /// t 8
    /// s 8 11
    /// d 0 1
    /// t 8 11
    /// ```
    pub(crate) fn file_replace(
        &mut self,
        idx: Option<usize>,
        name: Option<&[u8]>,
        source: Source,
        flags: u32,
    ) -> Done<usize> {
        if self.rdonly() {
            return self.fail(Er::Rdonly);
        }
        let before = self.entries.len();
        let idx = if let Some(idx) = idx {
            idx
        } else {
            let found = match name {
                Some(name) if flags & FL_OVERWRITE != 0 => self.locate(name, flags).ok(),
                _ => None,
            };
            found.unwrap_or_else(|| {
                self.entries.push(Entry::default());
                before
            })
        };
        if let Some(name) = name
            && self.set_name(idx, name, flags).is_err()
        {
            self.entries.truncate(before.max(idx.min(before)));
            return Err(Failed);
        }
        self.prepare_extra_change(idx)?;
        let e = &mut self.entries[idx];
        if let Some(c) = &mut e.changes {
            c.extra_central.clear();
            c.extra_local = Some(Vec::new());
        }
        unchange_data(e);
        e.source = Some(source);
        Ok(idx)
    }

    /// `_zip_file_extra_field_prepare_for_change`.
    fn prepare_extra_change(&mut self, idx: usize) -> Done {
        if self.entries[idx].changed(D_EXTRA_FIELD) {
            return Ok(());
        }
        if self.entries[idx].orig.is_some() && self.read_local_ef(idx).is_err() {
            return Err(Failed);
        }
        let e = &mut self.entries[idx];
        let c = e.changes.get_or_insert_with(|| clone_of(e.orig.as_ref()));
        c.changed |= D_EXTRA_FIELD;
        Ok(())
    }

    /// `zip_dir_add`.
    pub(crate) fn dir_add(&mut self, name: &[u8], flags: u32) -> Done<usize> {
        if self.rdonly() {
            return self.fail(Er::Rdonly);
        }
        let mut name = name.to_vec();
        if name.last() != Some(&b'/') {
            name.push(b'/');
        }
        let idx = self.file_replace(None, Some(&name), Source::buffer(Vec::new()), flags)?;
        if self
            .set_external_attributes(idx, OPSYS_DEFAULT, EXT_ATTRIB_DEFAULT_DIR)
            .is_err()
        {
            self.delete(idx).ok();
            return Err(Failed);
        }
        Ok(idx)
    }

    /// `zip_delete`.
    pub(crate) fn delete(&mut self, idx: usize) -> Done {
        if idx >= self.entries.len() {
            return self.fail(Er::Inval);
        }
        if self.rdonly() {
            return self.fail(Er::Rdonly);
        }
        self.get_name(idx, 0)?;
        self.unchange_one(idx, true)?;
        self.entries[idx].deleted = true;
        Ok(())
    }

    /// `_zip_unchange`.
    pub(crate) fn unchange_one(&mut self, idx: usize, allow_duplicates: bool) -> Done {
        if idx >= self.entries.len() {
            return self.fail(Er::Inval);
        }
        let e = &self.entries[idx];
        let renamed = e.changed(D_FILENAME);
        if !allow_duplicates && (renamed || e.deleted) && e.orig.is_some() {
            let orig_name = self.get_name(idx, FL_UNCHANGED)?;
            if let Ok(i) = self.locate(&orig_name, 0)
                && i != idx
            {
                return self.fail(Er::Exists);
            }
        }
        let e = &mut self.entries[idx];
        e.changes = None;
        unchange_data(e);
        Ok(())
    }

    /// `zip_unchange_all`.
    pub(crate) fn unchange_all(&mut self) -> Done {
        for i in 0..self.entries.len() {
            self.unchange_one(i, true)?;
        }
        self.unchange_archive();
        Ok(())
    }

    /// `zip_unchange_archive`.
    pub(crate) fn unchange_archive(&mut self) {
        self.comment_changes = None;
        self.comment_changed = false;
        self.ch_flags = self.flags;
    }

    /// `zip_file_rename`.
    pub(crate) fn rename(&mut self, idx: usize, name: &[u8]) -> Done {
        if idx >= self.entries.len() || name.len() > 0xffff {
            return self.fail(Er::Inval);
        }
        if self.rdonly() {
            return self.fail(Er::Rdonly);
        }
        let old = self.get_name(idx, 0)?;
        if (name.last() == Some(&b'/')) != (old.last() == Some(&b'/')) {
            return self.fail(Er::Inval);
        }
        self.set_name(idx, name, 0)
    }

    /// `zip_set_archive_comment`.
    pub(crate) fn set_archive_comment(&mut self, comment: &[u8]) -> Done {
        self.writable(true)?;
        let s = match ZStr::new(comment, 0) {
            Ok(Some(s)) if s.guess() == Enc::Cp437 => return self.fail(Er::Inval),
            Ok(s) => s,
            Err(()) => return self.fail(Er::Inval),
        };
        if self.comment_orig.as_ref().map(|c| &c.raw) == s.as_ref().map(|c| &c.raw) {
            self.comment_changes = None;
            self.comment_changed = false;
        } else {
            self.comment_changes = s;
            self.comment_changed = true;
        }
        Ok(())
    }

    /// `zip_get_archive_comment`.
    pub(crate) fn archive_comment(&self, flags: u32) -> Vec<u8> {
        let c = if flags & FL_UNCHANGED != 0 || self.comment_changes.is_none() {
            self.comment_orig.as_ref()
        } else {
            self.comment_changes.as_ref()
        };
        strings::get(c, flags).into_owned()
    }

    /// `zip_file_set_comment`.
    pub(crate) fn set_file_comment(&mut self, idx: usize, comment: &[u8], flags: u32) -> Done {
        if self.dirent(idx, 0).is_err() {
            return Err(Failed);
        }
        self.writable(true)?;
        let s = match ZStr::new(comment, flags) {
            Ok(s) if flags & (FL_ENC_UTF_8 | strings::FL_ENC_CP437) == 0 => s.map(ZStr::promoted),
            Ok(s) => s,
            Err(()) => return self.fail(Er::Inval),
        };
        let e = &mut self.entries[idx];
        if let Some(c) = &mut e.changes {
            c.comment = None;
            c.changed &= !D_COMMENT;
        }
        let changed = match e.orig.as_ref().and_then(|o| o.comment.as_ref()) {
            Some(orig) => s.as_ref().map(|s| &s.raw) != Some(&orig.raw),
            None => s.is_some(),
        };
        if changed {
            let c = e.changes.get_or_insert_with(|| clone_of(e.orig.as_ref()));
            c.comment = s;
            c.changed |= D_COMMENT;
        } else if e.changes.as_ref().is_some_and(|c| c.changed == 0) {
            e.changes = None;
        }
        Ok(())
    }

    /// `zip_file_get_comment`, `None` where `_zip_get_dirent` fails.
    pub(crate) fn file_comment(&self, idx: usize, flags: u32) -> Option<Vec<u8>> {
        let de = self.dirent(idx, flags).ok()?;
        Some(strings::get(de.comment.as_ref(), flags).into_owned())
    }

    /// `zip_file_set_external_attributes`.
    pub(crate) fn set_external_attributes(
        &mut self,
        idx: usize,
        opsys: u8,
        attributes: u32,
    ) -> Done {
        if self.dirent(idx, 0).is_err() {
            return Err(Failed);
        }
        self.writable(true)?;
        let e = &mut self.entries[idx];
        let unchanged_opsys = e
            .orig
            .as_ref()
            .map_or(OPSYS_DEFAULT, |o| (o.version_madeby >> 8) as u8);
        let unchanged_attributes = e.orig.as_ref().map_or(EXT_ATTRIB_DEFAULT, |o| o.ext_attrib);
        if opsys != unchanged_opsys || attributes != unchanged_attributes {
            let c = e.changes.get_or_insert_with(|| clone_of(e.orig.as_ref()));
            c.version_madeby = (u16::from(opsys) << 8) | (c.version_madeby & 0xff);
            c.ext_attrib = attributes;
            c.changed |= D_ATTRIBUTES;
        } else if let Some(c) = &mut e.changes {
            c.changed &= !D_ATTRIBUTES;
            if c.changed == 0 {
                e.changes = None;
            } else {
                c.version_madeby = (u16::from(unchanged_opsys) << 8) | (c.version_madeby & 0xff);
                c.ext_attrib = unchanged_attributes;
            }
        }
        Ok(())
    }

    /// `zip_file_get_external_attributes`.
    pub(crate) fn external_attributes(&self, idx: usize, flags: u32) -> Option<(u8, u32)> {
        let de = self.dirent(idx, flags).ok()?;
        Some(((de.version_madeby >> 8) as u8, de.ext_attrib))
    }

    /// `zip_set_file_compression`.
    pub(crate) fn set_compression(&mut self, idx: usize, method: i32, level: u32) -> Done {
        if idx >= self.entries.len() {
            return self.fail(Er::Inval);
        }
        self.writable(true)?;
        if !compression_supported(i64::from(method)) {
            return self.fail(Er::Compnotsupp);
        }
        let e = &mut self.entries[idx];
        let c = e.changes.get_or_insert_with(|| clone_of(e.orig.as_ref()));
        c.comp_method = method;
        c.compression_level = level & 0xffff;
        c.changed |= D_COMP_METHOD;
        Ok(())
    }

    /// `zip_file_set_encryption`.
    pub(crate) fn set_encryption(
        &mut self,
        idx: usize,
        method: u16,
        password: Option<&[u8]>,
    ) -> Done {
        if idx >= self.entries.len() {
            return self.fail(Er::Inval);
        }
        self.writable(true)?;
        if method != EM_NONE && !encryption_supported(i64::from(method)) {
            return self.fail(Er::Encrnotsupp);
        }
        let e = &mut self.entries[idx];
        let orig_password = e.orig.as_ref().and_then(|o| o.password.clone());
        let c = e.changes.get_or_insert_with(|| clone_of(e.orig.as_ref()));
        c.encryption_method = method;
        c.changed |= D_ENCRYPTION_METHOD;
        if let Some(password) = password {
            c.password = Some(password.to_vec());
            c.changed |= D_PASSWORD;
        } else if c.changed & D_PASSWORD != 0 {
            c.password = orig_password;
            c.changed &= !D_PASSWORD;
        }
        Ok(())
    }

    /// `zip_file_set_mtime`.
    pub(crate) fn set_mtime(&mut self, idx: usize, mtime: i64) -> Done {
        let dos = self.zone.u2d(mtime);
        if self.dirent(idx, 0).is_err() {
            return Err(Failed);
        }
        self.writable(true)?;
        let e = &self.entries[idx];
        if e.orig
            .as_ref()
            .is_some_and(|o| o.encryption_method == EM_TRAD_PKWARE)
            && !e.changed(D_ENCRYPTION_METHOD)
            && e.source.is_none()
        {
            return self.fail(Er::Opnotsupp);
        }
        let e = &mut self.entries[idx];
        let c = e.changes.get_or_insert_with(|| clone_of(e.orig.as_ref()));
        c.last_mod = dos;
        c.mtime = Some(mtime);
        c.changed |= D_LAST_MOD;
        Ok(())
    }

    /// `zip_set_archive_flag`.
    pub(crate) fn set_archive_flag(&mut self, flag: u32, value: bool) -> Done {
        if flag == AFL_IS_TORRENTZIP {
            return self.fail(Er::Inval);
        }
        let new_flags = if value {
            self.ch_flags | flag
        } else {
            self.ch_flags & !flag
        };
        if new_flags == self.ch_flags {
            return Ok(());
        }
        if self.flags & AFL_RDONLY != 0 {
            return self.fail(Er::Rdonly);
        }
        if flag & AFL_RDONLY != 0 && value && self.ch_flags & AFL_RDONLY == 0 && self.changed().0 {
            return self.fail(Er::Changed);
        }
        self.ch_flags = new_flags;
        Ok(())
    }

    /// `zip_get_archive_flag`.
    pub(crate) fn archive_flag(&self, flag: u32, flags: u32) -> bool {
        let fl = if flags & FL_UNCHANGED != 0 {
            self.flags
        } else {
            self.ch_flags
        };
        fl & flag != 0
    }

    /// `_zip_changed`: whether anything changed, and how many entries
    /// survive.
    pub(crate) fn changed(&self) -> (bool, usize) {
        let mut changed =
            self.comment_changed || (self.want_torrentzip() && self.flags & AFL_IS_TORRENTZIP == 0);
        let mut survivors = 0;
        for e in &self.entries {
            changed |= e.has_changes();
            survivors += usize::from(!e.deleted);
        }
        (changed, survivors)
    }

    /// `_zip_file_get_offset`: where entry `idx`'s data starts, past its
    /// local header as `_zip_dirent_size` reads it through the archive's
    /// `FILE`. A close that keeps every entry's data reads the last one's
    /// header for where the kept part ends, so a header cut short by the
    /// end of the file fails it, and one whose fixed part is whole but
    /// not a local header's is inconsistent:
    ///
    /// ```php
    /// function crafted($f, $back) {
    ///     $local = "PK\3\4" . pack('vvvvvVVVvv', 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 1, 0) . 'a' . 'hello';
    ///     $cd = '';
    ///     foreach (['a' => 0, 'b' => strlen($local) + 2 * 47 + 22 - $back] as $n => $off)
    ///         $cd .= "PK\1\2" . pack('vvvvvvVVVvvvvvVV', 0x0314, 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 1, 0, 0, 0, 0, 0x81a40000, $off) . $n;
    ///     file_put_contents($f, $local . $cd . "PK\5\6" . pack('vvvvVVv', 0, 0, 2, 2, strlen($cd), strlen($local), 0));
    /// }
    /// foreach ([1, 29, 30] as $back) {
    ///     crafted('end-witness.zip', $back); $z = new ZipArchive; $z->open('end-witness.zip'); $z->setArchiveComment('x');
    ///     echo "$back from the end: ", var_export(@$z->close(), true), " $z->status ", $z->getStatusString(), "\n";
    /// }
    /// unlink('end-witness.zip');
    /// ```
    /// ```expect capture
    /// 1 from the end: false 17 Premature end of file
    /// 29 from the end: false 17 Premature end of file
    /// 30 from the end: false 21 Zip archive inconsistent: entry 0: central header invalid
    /// ```
    pub(crate) fn data_offset(&mut self, idx: usize) -> Result<u64, Status> {
        let offset = self.entries[idx]
            .orig
            .as_ref()
            .ok_or(Status::new(Er::Internal))?
            .offset;
        let data = self.data.as_mut().ok_or(Status::new(Er::Internal))?;
        let len = data.size;
        let size = data
            .through(|f| Local::at(f, offset, len).size())
            .unwrap_or(Err(Status::new(Er::Open)))?;
        Ok(offset + size)
    }

    /// `_zip_read_local_ef`: the local header's extra fields, read from
    /// its lengths with no look at its signature, which the copy of the
    /// data holds it to after (`_zip_dirent_size`):
    ///
    /// ```php
    /// $b = "XX\3\4" . pack('vvvvvVVVvv', 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 1, 0) . 'ahello';
    /// $cd = "PK\1\2" . pack('vvvvvvVVVvvvvvVV', 0x314, 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 1, 0, 0, 0, 0, 0x81a40000, 0) . 'a';
    /// file_put_contents('magic-witness.zip', $b . $cd . "PK\5\6" . pack('vvvvVVv', 0, 0, 1, 1, strlen($cd), strlen($b), 0));
    /// $z = new ZipArchive; $z->open('magic-witness.zip'); $z->renameName('a', 'b');
    /// var_dump(@$z->close(), $z->getStatusString()); unlink('magic-witness.zip');
    /// ```
    /// ```expect
    /// bool(false)
    /// string(57) "Zip archive inconsistent: entry 0: central header invalid"
    /// ```
    ///
    /// The two lengths are sought to, so a header that starts so near the
    /// end of the file that they would lie past it is refused by the
    /// seek, and one whose lengths are cut short by the end is short:
    ///
    /// ```php
    /// function crafted($f, $back) {
    ///     $local = "PK\3\4" . pack('vvvvvVVVvv', 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 1, 0) . 'a' . 'hello';
    ///     $cd = '';
    ///     foreach (['a' => 0, 'b' => strlen($local) + 2 * 47 + 22 - $back] as $n => $off)
    ///         $cd .= "PK\1\2" . pack('vvvvvvVVVvvvvvVV', 0x0314, 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 1, 0, 0, 0, 0, 0x81a40000, $off) . $n;
    ///     file_put_contents($f, $local . $cd . "PK\5\6" . pack('vvvvVVv', 0, 0, 2, 2, strlen($cd), strlen($local), 0));
    /// }
    /// foreach ([1, 22, 26, 29] as $back) {
    ///     crafted('ef-end-witness.zip', $back); $z = new ZipArchive; $z->open('ef-end-witness.zip'); $z->renameName('a', 'c');
    ///     echo "$back from the end: ", var_export(@$z->close(), true), " $z->status ", $z->getStatusString(), "\n";
    /// }
    /// unlink('ef-end-witness.zip');
    /// ```
    /// ```expect capture
    /// 1 from the end: false 18 Invalid argument
    /// 22 from the end: false 18 Invalid argument
    /// 26 from the end: false 17 Premature end of file
    /// 29 from the end: false 17 Premature end of file
    /// ```
    pub(crate) fn read_local_ef(&mut self, idx: usize) -> Done {
        let Some(offset) = self.entries[idx]
            .orig
            .as_ref()
            .filter(|o| o.extra_local.is_none())
            .map(|o| o.offset)
        else {
            return Ok(());
        };
        let Some(data) = self.data.as_mut() else {
            return self.fail(Er::Internal);
        };
        let len = data.size;
        let read = data.through(|f| Local::at(f, offset, len).extra_fields());
        let extra = match read {
            Some(Ok(extra)) => extra,
            Some(Err(status)) => {
                self.error = status;
                return Err(Failed);
            }
            None => return self.fail(Er::Open),
        };
        if let Some(o) = &mut self.entries[idx].orig {
            o.extra_local = Some(extra.clone());
        }
        if let Some(c) = &mut self.entries[idx].changes
            && c.changed & D_EXTRA_FIELD == 0
        {
            c.extra_local = Some(extra);
        }
        Ok(())
    }

    /// The raw data of an unchanged entry, as stored: `copy_data`'s
    /// reads through the archive's `FILE`, 8192 bytes at a time, each
    /// short one `ZIP_ER_EOF`.
    ///
    /// What the `FILE` held at the open is copied after the file is cut:
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('copy-witness.zip', ZipArchive::CREATE|ZipArchive::OVERWRITE);
    /// $z->addFromString('a', str_repeat('a', 3000)); $z->setCompressionName('a', ZipArchive::CM_STORE); $z->addFromString('b', 'bee'); $z->close();
    /// $z->open('copy-witness.zip'); $h = fopen('copy-witness.zip', 'r+'); ftruncate($h, 60); fclose($h);
    /// $z->renameName('a', 'A'); var_dump($z->close(), $z->open('copy-witness.zip'), strlen($z->getFromName('A')), $z->getFromName('b'));
    /// $z->close(); $z->open('copy-witness.zip'); $h = fopen('copy-witness.zip', 'r+'); ftruncate($h, 100); fclose($h);
    /// $z->renameName('A', 'a'); var_dump(@$z->close(), $z->getStatusString()); unlink('copy-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// int(3000)
    /// string(3) "bee"
    /// bool(true)
    /// string(8) "No error"
    /// ```
    pub(crate) fn raw_data(&mut self, idx: usize) -> Result<Vec<u8>, Status> {
        let size = self.entries[idx].orig.as_ref().map_or(0, |o| o.comp_size);
        let start = self.data_offset(idx)?;
        let data = self.data.as_mut().ok_or(Status::new(Er::Internal))?;
        data.through(|f| {
            f.seek(start);
            let mut out = Vec::new();
            while (out.len() as u64) < size {
                let at = out.len();
                let n = usize::try_from(size - at as u64).map_or(8192, |n| n.min(8192));
                out.resize(at + n, 0);
                match f.read(&mut out[at..]) {
                    Ok(k) if k == n => {}
                    Ok(_) => return Err(Status::new(Er::Eof)),
                    Err(e) => return Err(Status::os(Er::Read, &e)),
                }
            }
            Ok(out)
        })
        .unwrap_or(Err(Status::new(Er::Open)))
    }

    /// The window on the archive's file that entry `idx`'s data is, past
    /// its local header (`_zip_file_get_offset`), as libzip 1.12 makes it:
    /// refused with `ZIP_ER_INVAL` where the data would pass the archive's
    /// size at its open, then the whole header read through the archive's
    /// `FILE` from a seek to it, a file too short for it `ZIP_ER_EOF` and a
    /// header without its signature the bare detail `central header
    /// invalid`. Data that is empty takes no window, nor does a
    /// decryption layer's own refusal (`Reader::decoded`):
    ///
    /// ```php
    /// function zip1($data, $flags = 0, $magic = "PK\3\4", $csize = null) {
    ///     $l = $magic . pack('vvvvvVVVvv', 20, $flags, 0, 0, 0x21, crc32($data), strlen($data), strlen($data), 1, 0) . 'a' . $data;
    ///     $c = "PK\1\2" . pack('vvvvvvVVVvvvvvVV', 0x0314, 20, $flags, 0, 0, 0x21, crc32($data), $csize ?? strlen($data), strlen($data), 1, 0, 0, 0, 0, 0x81a40000, 0) . 'a';
    ///     return $l . $c . "PK\5\6" . pack('vvvvVVv', 0, 0, 1, 1, strlen($c), strlen($l), 0);
    /// }
    /// $cases = ['bad magic' => zip1('hello', 0, "XX\3\4"), 'past the end' => zip1('hello', 0, "PK\3\4", 100000),
    ///     'short pkware' => zip1('hello', 1), 'empty pkware' => zip1('', 1)];
    /// foreach ($cases as $label => $bytes) {
    ///     file_put_contents('window-witness.zip', $bytes); $z = new ZipArchive; $z->open('window-witness.zip'); $z->setPassword('pw');
    ///     $r = @$z->getFromName('a'); echo $label, ': ', var_export($r, true), ' ', $z->getStatusString(), ' ', var_export(@$z->getStream('a'), true), "\n";
    ///     $z->close();
    /// }
    /// unlink('window-witness.zip');
    /// ```
    /// ```expect
    /// bad magic: false Zip archive inconsistent: entry 0: central header invalid false
    /// past the end: false Invalid argument false
    /// short pkware: false Operation not supported false
    /// empty pkware: false Operation not supported false
    /// ```
    fn window(&mut self, idx: usize) -> Result<Window, Status> {
        let orig = self.entries[idx]
            .orig
            .as_ref()
            .ok_or(Status::new(Er::Internal))?;
        let (offset, size) = (orig.offset, orig.comp_size);
        let data = self.data.as_mut().ok_or(Status::new(Er::Internal))?;
        // `_zip_source_window_new` (libzip 1.12): data past the archive's
        // size at its open is refused as the window is made.
        if size > data.size {
            return Err(Status::new(Er::Inval));
        }
        let mut own = data
            .file
            .try_clone()
            .map(Stdio::new)
            .map_err(|e| Status::os(Er::Open, &e))?;
        let f = data.stdio.as_mut().unwrap_or(&mut own);
        let start = offset + Local::at(f, offset, data.size).size()?;
        Ok(Window::new(own, start, size, true))
    }

    /// An unchanged entry's data opened for reading whole: decrypted with
    /// `password` where one is given, decompressed, its CRC checked at
    /// the end where `stat` has one (`zip_source_zip_file_create`).
    fn decoder(
        &mut self,
        idx: usize,
        flags: u32,
        password: Option<&[u8]>,
        st: &Stat,
        check_crc: bool,
    ) -> Result<Reader, Status> {
        let de = self.dirent(idx, flags).map_err(Status::new)?;
        let (dostime, bitflags) = (de.last_mod.time, de.bitflags);
        let crc_valid = self.entries[idx].orig.as_ref().is_some_and(|o| o.crc_valid);
        let layers = Layers {
            method: st.comp_method,
            encryption: st.encryption_method,
            password,
            size: st.size,
            comp_size: st.comp_size,
            crc: (check_crc && crc_valid).then_some(st.crc),
            dostime,
            bitflags,
            check: self.open_flags & CHECKCONS != 0,
        };
        Reader::decoded(|| self.window(idx), &layers)
    }

    /// An unchanged entry's data, decrypted with `password` and
    /// decompressed, and the error that stopped the read short. Each of
    /// two entries under one name reads its own data.
    ///
    /// ```php
    /// $f = 'dup-witness.zip'; $z = new ZipArchive; $z->open($f, ZipArchive::CREATE);
    /// $z->addFromString('a', 'first'); $z->addFromString('b', str_repeat('second ', 9)); $z->close();
    /// file_put_contents($f, str_replace("\0b", "\0a", file_get_contents($f)));
    /// var_dump($z->open($f), $z->numFiles, $z->getNameIndex(1), $z->getFromIndex(0), $z->getFromIndex(1), $z->getFromName('a'));
    /// echo stream_get_contents($z->getStreamIndex(1)), "\n";
    /// $z->close(); unlink($f);
    /// ```
    /// ```expect
    /// bool(true)
    /// int(2)
    /// string(1) "a"
    /// string(5) "first"
    /// string(63) "second second second second second second second second second "
    /// string(5) "first"
    /// second second second second second second second second second
    /// ```
    pub(crate) fn decoded(
        &mut self,
        idx: usize,
        password: Option<&[u8]>,
    ) -> Result<(Vec<u8>, Option<Status>), Status> {
        let st = self
            .stat_index(idx, FL_UNCHANGED)
            .map_err(|Failed| Status::new(Er::Internal))?;
        if st.encryption_method != EM_NONE && password.is_none() {
            return Err(Status::new(Er::Nopasswd));
        }
        let mut reader = self.decoder(idx, FL_UNCHANGED, password, &st, false)?;
        Ok(match &mut self.data {
            Some(d) => reader.read_to_end(&mut d.stdio),
            None => reader.read_to_end(&mut None),
        })
    }

    /// `file` swapped with the archive's `FILE`: lent to it, and taken
    /// back by the same call.
    pub(crate) fn lend(&mut self, file: &mut Option<Stdio>) {
        if let Some(d) = &mut self.data {
            std::mem::swap(&mut d.stdio, file);
        }
    }

    /// `zip_fread` on an entry this archive opened, through its `FILE`.
    pub(crate) fn read_entry(&mut self, zf: &mut Reader, len: usize) -> Result<Vec<u8>, Status> {
        match &mut self.data {
            Some(d) => zf.read_through(len, &mut d.stdio),
            None => zf.read(len),
        }
    }

    /// `zip_fopen_index_encrypted`: entry `idx` opened for reading,
    /// decrypted with `password` or the archive's, decompressed as it is
    /// read, its CRC checked at the end.
    pub(crate) fn fopen(
        &mut self,
        idx: usize,
        flags: u32,
        password: Option<&[u8]>,
    ) -> Done<Reader> {
        let password = password.filter(|p| !p.is_empty()).map(<[u8]>::to_vec);
        let Some(entry) = self.entries.get(idx) else {
            return self.fail(Er::Inval);
        };
        let flags = if flags & FL_ENCRYPTED != 0 {
            flags | FL_COMPRESSED
        } else {
            flags
        };
        let changed_data = flags & FL_UNCHANGED == 0 && entry.source.is_some();
        if flags & FL_UNCHANGED == 0
            && (if changed_data {
                flags & FL_COMPRESSED != 0
            } else {
                entry.deleted
            })
        {
            return self.fail(Er::Changed);
        }
        let stat_flags = if changed_data {
            flags
        } else {
            flags | FL_UNCHANGED
        };
        let Ok(st) = self.stat_index(idx, stat_flags) else {
            return self.fail(Er::Internal);
        };
        let encrypted = st.encryption_method != EM_NONE;
        let needs_decrypt = flags & FL_ENCRYPTED == 0 && encrypted;
        let password = password.or_else(|| self.default_password.clone());
        if needs_decrypt && password.is_none() {
            return self.fail(Er::Nopasswd);
        }
        if !changed_data && flags & FL_COMPRESSED != 0 && needs_decrypt {
            return self.fail(Er::Encrnotsupp);
        }
        let crc_valid = self.entries[idx].orig.as_ref().is_some_and(|o| o.crc_valid);
        let opened = if changed_data {
            let opened = self.entries[idx].source.as_ref().map(Source::reader);
            opened.unwrap_or(Err(Status::new(Er::Internal)))
        } else if flags & FL_COMPRESSED != 0 {
            let crc = (st.comp_method == 0 && crc_valid).then_some((st.crc, st.size));
            self.window(idx).map(|w| Reader::window(w, crc))
        } else {
            let password = if needs_decrypt {
                password.as_deref()
            } else {
                None
            };
            self.decoder(idx, flags, password, &st, true)
        };
        opened.map_err(|status| {
            self.error = status;
            Failed
        })
    }
}

/// `_zip_unchange_data`.
fn unchange_data(e: &mut Entry) {
    e.source = None;
    e.deleted = false;
}

pub(crate) fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The extra fields libzip makes itself and drops on reading: Zip64,
/// the two UTF-8 fields and `WinZip` AES (`_zip_ef_remove_internal`).
pub(crate) fn strip_internal(ef: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0;
    while let (Some(id), Some(len)) = (u16_at(ef, at), u16_at(ef, at + 2)) {
        let end = at + 4 + usize::from(len);
        let Some(field) = ef.get(at..end) else {
            break;
        };
        if !matches!(id, 0x0001 | 0x7075 | 0x6375 | 0x9901) {
            out.extend_from_slice(field);
        }
        at = end;
    }
    out
}

/// The data of extra field `id`.
pub(crate) fn find_ef(ef: &[u8], id: u16) -> Option<&[u8]> {
    let mut at = 0;
    while let (Some(fid), Some(len)) = (u16_at(ef, at), u16_at(ef, at + 2)) {
        let data = ef.get(at + 4..at + 4 + usize::from(len))?;
        if fid == id {
            return Some(data);
        }
        at += 4 + usize::from(len);
    }
    None
}

/// `zip_check_torrentzip`: the comment `TORRENTZIPPED-` and the central
/// directory's CRC-32 in upper-case hex.
fn is_torrentzip(comment: &[u8], cdir: &[u8]) -> bool {
    let Some(hex) = comment.strip_prefix(b"TORRENTZIPPED-") else {
        return false;
    };
    if hex.len() != 8
        || !hex
            .iter()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(b))
    {
        return false;
    }
    let Ok(text) = std::str::from_utf8(hex) else {
        return false;
    };
    u32::from_str_radix(text, 16).is_ok_and(|crc| crc == crc32fast::hash(cdir))
}
