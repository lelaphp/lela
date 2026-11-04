//! `phar_internal.h`'s model and `util.c`'s table of open archives: an
//! archive ([`Archive`], `phar_archive_data`) with its manifest of entries
//! ([`Entry`], `phar_entry_info`), and the request's table of them by
//! file name and by alias (`PHAR_G(phar_fname_map)`, `phar_alias_map`),
//! with php's reference counting, which decides when an archive is let go
//! and so whether `unlinkArchive()` refuses and whether a second open
//! reads the file again.
//!
//! An entry is read from one of the four `phar_fp_type`s: `fp`, the
//! archive's file as a live stream, seeked and read per read as php's is,
//! or the bytes php wrote into a stream it holds; the buffer entries are
//! decompressed into (`ufp`); a buffer of the entry's own once it is
//! written (`PHAR_MOD`); or the stream of a mounted file (`PHAR_TMP`).

use std::sync::Arc;

use php_ext::raising::Thrown;
use php_ext::request::{
    MAXPATHLEN, Opening, OwnedStream as _, Request, Stream, Streams as _, Whence,
};
use php_ext::values::{Owner, PhpString, Value};

use crate::entry::Efp;
use crate::table::Table;

pub(crate) const API_VERSION: &str = "1.1.1";
/// `PHAR_API_VERSION`, x.y.z as `0xxyz0`, and the one written for an
/// archive without directories.
pub(crate) const API_VERSION_NUM: u16 = 0x1110;
pub(crate) const API_VERSION_NODIR: u16 = 0x1100;
pub(crate) const API_MIN_DIR: u16 = 0x1110;
pub(crate) const API_MIN_READ: u16 = 0x1000;
pub(crate) const API_VER_MASK: u16 = 0xFFF0;

pub(crate) const HDR_COMPRESSION_MASK: u32 = 0x0000_F000;
pub(crate) const HDR_SIGNATURE: u32 = 0x0001_0000;
pub(crate) const FILE_COMPRESSION_MASK: u32 = 0x00F0_0000;
pub(crate) const FILE_COMPRESSED_GZ: u32 = 0x0010_0000;
pub(crate) const FILE_COMPRESSED_BZ2: u32 = 0x0020_0000;

pub(crate) const SIG_MD5: u32 = 0x0001;
pub(crate) const SIG_SHA1: u32 = 0x0002;
pub(crate) const SIG_SHA256: u32 = 0x0003;
pub(crate) const SIG_SHA512: u32 = 0x0004;
pub(crate) const SIG_OPENSSL: u32 = 0x0010;
pub(crate) const SIG_OPENSSL_SHA256: u32 = 0x0011;
pub(crate) const SIG_OPENSSL_SHA512: u32 = 0x0012;

pub(crate) const ENT_COMPRESSION_MASK: u32 = 0x0000_F000;
pub(crate) const ENT_COMPRESSED_GZ: u32 = 0x0000_1000;
pub(crate) const ENT_COMPRESSED_BZ2: u32 = 0x0000_2000;
pub(crate) const ENT_PERM_MASK: u32 = 0x0000_01FF;
pub(crate) const ENT_PERM_DEF_FILE: u32 = 0x0000_01B6;
pub(crate) const ENT_PERM_DEF_DIR: u32 = 0x0000_01FF;

/// `TAR_FILE`, `TAR_LINK`, `TAR_SYMLINK` and `TAR_DIR`, the type bytes a
/// tar header carries and a tar entry keeps.
pub(crate) const TAR_FILE: u8 = b'0';
pub(crate) const TAR_LINK: u8 = b'1';
pub(crate) const TAR_SYMLINK: u8 = b'2';
pub(crate) const TAR_DIR: u8 = b'5';

/// The archive's format: `is_tar` and `is_zip`, neither for the phar
/// format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Phar,
    Tar,
    Zip,
}

/// Where an entry's contents are read from, `phar_fp_type` with the
/// stream it names.
pub(crate) enum Content {
    /// `PHAR_FP`: `phar->fp`, at the entry's `offset`.
    Fp,
    /// `PHAR_UFP`: the archive's decompressed buffer, at `offset`.
    Ufp,
    /// `PHAR_MOD`: a buffer of the entry's own, which a write made.
    Mod(Vec<u8>),
    /// `PHAR_TMP`: the file `tmp` names, mounted, opened when first read,
    /// the entry holding its owner.
    Tmp(Option<Owner<Stream>>),
}

/// `phar->fp`, the stream an uncompressed entry is read from.
pub(crate) enum Fp {
    /// The file opened `rb`, as an open keeps it and
    /// `phar_open_archive_fp` opens it again: what is read through it is
    /// the file as it stands, rewritten since or unlinked, and a write
    /// through it fails.
    File(Owner<Stream>),
    /// A stream php wrote the bytes into, the file the flush opened `w+b`
    /// or a temporary one (a whole-file compressed archive's decompressed
    /// copy, a deferred flush's): a write through it lands. Shared with
    /// each entry stream opened on them, which reads them on after a write
    /// replaces them, as a php stream keeps its own `fp`.
    Written(Arc<Vec<u8>>),
}

impl Fp {
    /// What an entry reads through this `fp`.
    pub(crate) fn efp(&self) -> Efp {
        match self {
            Fp::File(stream) => Efp::Stream(**stream),
            Fp::Written(bytes) => Efp::Shared(Arc::clone(bytes)),
        }
    }
}

/// `phar_metadata_tracker`: the value `setMetadata()` was given, the
/// serialised string, or both; an owner of the value.
#[derive(Default)]
pub(crate) struct Metadata {
    pub(crate) val: Option<Owner<Value>>,
    pub(crate) str: Option<Vec<u8>>,
}

impl Metadata {
    /// `phar_metadata_tracker_has_data`.
    pub(crate) fn has_data(&self) -> bool {
        self.val.is_some() || self.str.is_some()
    }

    /// `phar_metadata_tracker_free`: the string first, then the value.
    pub(crate) fn free(&mut self, req: &mut Request) {
        self.str = None;
        if let Some(val) = self.val.take() {
            val.give_up(req);
        }
    }

    /// `phar_metadata_tracker_copy`: another owner of the value, a copy
    /// of the string.
    pub(crate) fn copied(&self, req: &Request) -> Metadata {
        Metadata {
            val: self.val.as_deref().map(|val| Owner::take(req, *val)),
            str: self.str.clone(),
        }
    }
}

/// `phar_entry_info`.
#[allow(clippy::struct_excessive_bools)] // php's bit fields, one flag a bit
pub(crate) struct Entry {
    pub(crate) filename: Vec<u8>,
    pub(crate) uncompressed_filesize: u32,
    pub(crate) timestamp: u32,
    pub(crate) compressed_filesize: u32,
    pub(crate) crc32: u32,
    pub(crate) flags: u32,
    pub(crate) old_flags: u32,
    pub(crate) metadata: Metadata,
    pub(crate) content: Content,
    /// Where the contents stand in the stream `content` names.
    pub(crate) offset: i64,
    pub(crate) offset_abs: i64,
    pub(crate) header_offset: i64,
    pub(crate) fp_refcount: i32,
    pub(crate) fileinfo_lock_count: u32,
    /// The mounted path, `entry->tmp`.
    pub(crate) tmp: Option<Vec<u8>>,
    /// A tar entry's link target, `entry->link`.
    pub(crate) link: Option<Vec<u8>>,
    pub(crate) tar_type: u8,
    pub(crate) inode: u16,
    pub(crate) is_crc_checked: bool,
    pub(crate) is_modified: bool,
    pub(crate) is_deleted: bool,
    pub(crate) is_dir: bool,
    pub(crate) is_mounted: bool,
    /// A directory made up for a path under which entries stand, never
    /// in the manifest.
    pub(crate) is_temp_dir: bool,
}

impl Entry {
    /// `destroy_phar_manifest_entry`: the entry's `metadata` freed and a mounted
    /// file's stream closed.
    pub(crate) fn free(mut self, req: &mut Request) {
        self.metadata.free(req);
        if let Content::Tmp(Some(stream)) = self.content {
            stream.free(req);
        }
    }

    /// An entry with every field zero, `memset(&entry, 0, ...)`.
    pub(crate) fn new(filename: &[u8]) -> Entry {
        Entry {
            filename: filename.to_vec(),
            uncompressed_filesize: 0,
            timestamp: 0,
            compressed_filesize: 0,
            crc32: 0,
            flags: 0,
            old_flags: 0,
            metadata: Metadata::default(),
            content: Content::Fp,
            offset: 0,
            offset_abs: 0,
            header_offset: 0,
            fp_refcount: 0,
            fileinfo_lock_count: 0,
            tmp: None,
            link: None,
            tar_type: 0,
            inode: 0,
            is_crc_checked: false,
            is_modified: false,
            is_deleted: false,
            is_dir: false,
            is_mounted: false,
            is_temp_dir: false,
        }
    }

    /// `memcpy(&newentry, oldentry, ...)` under a new name, as
    /// `Phar::copy()` makes its row: every field but the `metadata`, which
    /// the caller copies, the reader count and a mounted file's stream,
    /// which the copy opens again.
    pub(crate) fn shallow(&self, filename: &[u8]) -> Entry {
        Entry {
            filename: filename.to_vec(),
            metadata: Metadata::default(),
            content: match &self.content {
                Content::Fp => Content::Fp,
                Content::Ufp => Content::Ufp,
                Content::Mod(bytes) => Content::Mod(bytes.clone()),
                Content::Tmp(_) => Content::Tmp(None),
            },
            tmp: self.tmp.clone(),
            link: self.link.clone(),
            fp_refcount: 0,
            ..*self
        }
    }

    /// `phar_entry_can_remove`.
    pub(crate) fn can_remove(&self) -> bool {
        self.fp_refcount == 0 && self.fileinfo_lock_count == 0
    }

    /// `phar_set_inode`: `zend_hash_func` over the archive's name and,
    /// after it, as many bytes as the entry's name is long, cut to 16
    /// bits. php copies those bytes from the entry's `zend_string` itself
    /// rather than its characters (`memcpy(tmp + len1, entry->filename,
    /// len2)`), so they are the string's header, its `refcount`, its
    /// `type_info` (`GC_STRING`), its cached hash (zero until a table
    /// hashed it) and its length, before the characters from the 25th
    /// byte on; `refcount` and `hashed` are what the caller's string
    /// holds.
    pub(crate) fn set_inode(&mut self, fname: &[u8], refcount: u32, hashed: bool) {
        let name = &self.filename;
        let total = (name.len() + fname.len()).min(MAXPATHLEN);
        let first = fname.len().min(total);
        let second = (total - first).min(name.len());
        let mut header = Vec::with_capacity(24 + name.len());
        header.extend_from_slice(&refcount.to_le_bytes());
        header.extend_from_slice(&0x16_u32.to_le_bytes());
        let h = if hashed { zend_hash(name) } else { 0 };
        header.extend_from_slice(&h.to_le_bytes());
        header.extend_from_slice(&(name.len() as u64).to_le_bytes());
        header.extend_from_slice(name);
        let hash = zend_hash(&[&fname[..first], &header[..second]].concat());
        self.inode = u16::try_from(hash & 0xFFFF).unwrap_or(0);
    }
}

/// `zend_inline_hash_func`: DJBX33A with the top bit set.
pub(crate) fn zend_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 5381;
    for byte in bytes {
        hash = hash.wrapping_mul(33).wrapping_add(u64::from(*byte));
    }
    hash | 0x8000_0000_0000_0000
}

/// An open archive's place in the request's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArchiveId(pub(crate) usize);

/// `phar_archive_data`.
#[allow(clippy::struct_excessive_bools)] // php's bit fields, one flag a bit
pub(crate) struct Archive {
    pub(crate) fname: Vec<u8>,
    /// The length of the extension at the end of `fname`, `ext_len`.
    pub(crate) ext_len: usize,
    pub(crate) alias: Option<Vec<u8>>,
    pub(crate) is_temporary_alias: bool,
    pub(crate) version: String,
    pub(crate) halt_offset: usize,
    pub(crate) manifest: Table<Entry>,
    pub(crate) virtual_dirs: Table<()>,
    pub(crate) mounted_dirs: Table<()>,
    pub(crate) flags: u32,
    pub(crate) min_timestamp: u32,
    pub(crate) max_timestamp: u32,
    /// The archive's bytes, decompressed where the whole file is, and the
    /// stream php holds them through.
    pub(crate) fp: Option<Fp>,
    /// File streams a flush replaced in `fp`, which an entry stream open
    /// then reads on, closed at the archive's last reference; php keeps
    /// such a stream too (`free_fp`, `Z/ext/phar/phar.c:2695`).
    pub(crate) retired: Vec<Owner<Stream>>,
    /// Where compressed entries are decompressed to, shared the same way.
    pub(crate) ufp: Option<Arc<Vec<u8>>>,
    pub(crate) refcount: i32,
    pub(crate) sig_flags: u32,
    /// The signature as upper-case hex.
    pub(crate) signature: Option<Vec<u8>>,
    pub(crate) metadata: Metadata,
    pub(crate) format: Format,
    pub(crate) is_modified: bool,
    pub(crate) is_writeable: bool,
    pub(crate) is_brandnew: bool,
    pub(crate) donotflush: bool,
    pub(crate) is_data: bool,
}

impl Archive {
    /// An archive with every field zero.
    pub(crate) fn new(fname: &[u8], format: Format) -> Archive {
        Archive {
            fname: fname.to_vec(),
            ext_len: 0,
            alias: None,
            is_temporary_alias: false,
            version: String::new(),
            halt_offset: 0,
            manifest: Table::default(),
            virtual_dirs: Table::default(),
            mounted_dirs: Table::default(),
            flags: 0,
            min_timestamp: 0,
            max_timestamp: 0,
            fp: None,
            retired: Vec::new(),
            ufp: None,
            refcount: 0,
            sig_flags: 0,
            signature: None,
            metadata: Metadata::default(),
            format,
            is_modified: false,
            is_writeable: false,
            is_brandnew: false,
            donotflush: false,
            is_data: false,
        }
    }

    pub(crate) fn is_tar(&self) -> bool {
        self.format == Format::Tar
    }

    pub(crate) fn is_zip(&self) -> bool {
        self.format == Format::Zip
    }

    /// The extension as the parsers find it in the name: after the last
    /// `/`, from the first `.` that does not begin the base name.
    pub(crate) fn find_ext(&mut self) {
        self.ext_len = 0;
        let Some(slash) = self.fname.iter().rposition(|b| *b == b'/') else {
            return;
        };
        let tail = &self.fname[slash..];
        let mut dot = tail.iter().position(|b| *b == b'.');
        if dot == Some(0) {
            dot = tail[1..].iter().position(|b| *b == b'.').map(|d| d + 1);
        }
        if let Some(dot) = dot {
            self.ext_len = tail.len() - dot;
        }
    }

    /// `phar_add_virtual_dirs`: every directory above `filename`, until
    /// one already there.
    pub(crate) fn add_virtual_dirs(&mut self, filename: &[u8]) {
        let mut len = filename.len();
        while let Some(slash) = filename[..len].iter().rposition(|b| *b == b'/') {
            len = slash;
            if len == 0 || self.virtual_dirs.add(&filename[..len], ()).is_err() {
                break;
            }
        }
    }

    /// `phar->fp` set to `fp`, a file stream it held kept for the entry
    /// streams that read it.
    pub(crate) fn set_fp(&mut self, fp: Fp) {
        if let Some(Fp::File(stream)) = self.fp.replace(fp) {
            self.retired.push(stream);
        }
    }

    /// What `phar_destroy_phar_data` frees that the request holds: the
    /// `metadata` values, the archive's streams and the mounted files'.
    pub(crate) fn release_all(&mut self, req: &mut Request) {
        self.metadata.free(req);
        if let Some(Fp::File(stream)) = self.fp.take() {
            stream.free(req);
        }
        for stream in std::mem::take(&mut self.retired) {
            stream.free(req);
        }
        for entry in self.manifest.clear() {
            entry.free(req);
        }
    }
}

/// `PHAR_G(last_phar)` and its keys: the archive `phar_get_archive` found
/// last, by the name and the alias it answered.
#[derive(Debug, Clone)]
pub(crate) struct Last {
    pub(crate) id: ArchiveId,
    pub(crate) name: Vec<u8>,
    pub(crate) alias: Option<Vec<u8>>,
}

/// The request's archives: `phar_fname_map` and `phar_alias_map` over one
/// table of archives, and what else of `zend_phar_globals` the stream
/// wrapper reads.
#[derive(Default)]
pub(crate) struct Archives {
    slots: Vec<Option<Archive>>,
    pub(crate) fname_map: Table<ArchiveId>,
    pub(crate) alias_map: Table<ArchiveId>,
    pub(crate) last: Option<Last>,
    /// `PHAR_G(request_done)`.
    pub(crate) request_done: bool,
}

impl Archives {
    pub(crate) fn get(&self, id: ArchiveId) -> Option<&Archive> {
        self.slots.get(id.0)?.as_ref()
    }

    pub(crate) fn get_mut(&mut self, id: ArchiveId) -> Option<&mut Archive> {
        self.slots.get_mut(id.0)?.as_mut()
    }

    /// A new archive in the table, in no map yet.
    pub(crate) fn insert(&mut self, archive: Archive) -> ArchiveId {
        self.slots.push(Some(archive));
        ArchiveId(self.slots.len() - 1)
    }

    /// The archive taken out of the table, `phar_destroy_phar_data`'s
    /// free; its values are the caller's to release.
    pub(crate) fn take(&mut self, id: ArchiveId) -> Option<Archive> {
        self.slots.get_mut(id.0)?.take()
    }

    /// The archive registered under `fname`.
    pub(crate) fn by_fname(&self, fname: &[u8]) -> Option<ArchiveId> {
        self.fname_map.get(fname).copied()
    }

    /// The archive registered under `alias`.
    pub(crate) fn by_alias(&self, alias: &[u8]) -> Option<ArchiveId> {
        self.alias_map.get(alias).copied()
    }

    /// Every live archive, for the ini handler that sets their writable
    /// bit.
    pub(crate) fn each_mut(&mut self) -> impl Iterator<Item = &mut Archive> {
        self.slots.iter_mut().flatten()
    }
}

/// The request's phar state.
pub(crate) fn archives(req: &mut Request) -> &mut Archives {
    &mut req.state::<crate::State>().archives
}

/// `phar_destroy_phar_data`: the archive let go, its values released.
pub(crate) fn destroy(req: &mut Request, id: ArchiveId) {
    if let Some(mut archive) = archives(req).take(id) {
        archive.release_all(req);
    }
}

/// `zend_hash_str_del(&phar_fname_map, fname)` with the map's destructor,
/// `destroy_phar_data`: the aliases naming the archive dropped, then the
/// archive let go where no reference is left. `false` where no archive
/// had the name.
pub(crate) fn fname_map_del(req: &mut Request, fname: &[u8]) -> bool {
    let table = archives(req);
    let Some(id) = table.fname_map.remove(fname) else {
        return false;
    };
    for alias in table.alias_map.keys() {
        if table.alias_map.get(&alias) == Some(&id) {
            table.alias_map.remove(&alias);
        }
    }
    let gone = table.get_mut(id).is_some_and(|a| {
        a.refcount -= 1;
        a.refcount < 0
    });
    if gone {
        destroy(req, id);
    }
    true
}

/// `phar_archive_delref`: one reference fewer; at none left the archive's
/// file is closed where that is safe, and an archive nothing was added to,
/// or one compressed whole, is dropped from the name table. `true` where the
/// archive was let go.
pub(crate) fn delref(req: &mut Request, id: ArchiveId) -> bool {
    let done = archives(req).request_done;
    let Some(archive) = archives(req).get_mut(id) else {
        return false;
    };
    archive.refcount -= 1;
    if archive.refcount < 0 {
        let fname = archive.fname.clone();
        if done || !fname_map_del(req, &fname) {
            destroy(req, id);
        }
        return true;
    }
    if archive.refcount == 0 {
        let mut remove = archive.manifest.is_empty();
        let compressed = archive.flags & FILE_COMPRESSION_MASK != 0;
        let mut closing = std::mem::take(&mut archive.retired);
        if archive.fp.is_some() && (!compressed || archive.alias.is_none()) {
            if let Some(Fp::File(stream)) = archive.fp.take() {
                closing.push(stream);
            }
            remove |= compressed;
        }
        let fname = archive.fname.clone();
        archives(req).last = None;
        for stream in closing {
            stream.free(req);
        }
        if remove {
            if !fname_map_del(req, &fname) {
                destroy(req, id);
            }
            return true;
        }
    }
    false
}

/// `phar_free_alias`: an archive nothing holds dropped from the name table
/// so its alias may be taken; `false` where something holds it.
pub(crate) fn free_alias(req: &mut Request, id: ArchiveId) -> bool {
    let Some(archive) = archives(req).get(id) else {
        return false;
    };
    if archive.refcount != 0 {
        return false;
    }
    let fname = archive.fname.clone();
    if !fname_map_del(req, &fname) {
        return false;
    }
    archives(req).last = None;
    true
}

/// `phar_get_archive`: the archive registered under a name or an alias,
/// with php's lookup order and its last-found cache, and the alias bound
/// to it where one is given. The `Err` is php's error message, `None`
/// where php sets none.
pub(crate) fn get_archive(
    req: &mut Request,
    fname: &[u8],
    alias: Option<&[u8]>,
) -> Result<ArchiveId, Option<String>> {
    let alias = alias.filter(|a| !a.is_empty());
    let table = archives(req);
    if let Some(last) = table.last.clone()
        && fname == last.name.as_slice()
    {
        if let Some(alias) = alias {
            let (temporary, current, name) = match table.get(last.id) {
                Some(a) => (a.is_temporary_alias, a.alias.clone(), a.fname.clone()),
                None => return Err(None),
            };
            if !temporary && current.as_deref() != Some(alias) {
                return Err(Some(format!(
                    "alias \"{}\" is already used for archive \"{}\" cannot be overloaded with \"{}\"",
                    lossy(alias),
                    lossy(&name),
                    lossy(fname)
                )));
            }
            if let Some(current) = current
                && table.alias_map.contains(&current)
            {
                table.alias_map.remove(&current);
            }
            table.alias_map.add(alias, last.id).ok();
            if let Some(cache) = &mut table.last {
                cache.alias = Some(alias.to_vec());
            }
        }
        return Ok(last.id);
    }
    let cached_alias = table
        .last
        .as_ref()
        .filter(|last| alias.is_some() && last.alias.as_deref() == alias)
        .map(|last| last.id);
    let by_alias = cached_alias.or_else(|| alias.and_then(|a| table.by_alias(a)));
    if let (Some(alias), Some(id)) = (alias, by_alias) {
        let name = table.get(id).map(|a| a.fname.clone()).unwrap_or_default();
        if !fname.is_empty() && fname != name.as_slice() {
            let message = format!(
                "alias \"{}\" is already used for archive \"{}\" cannot be overloaded with \"{}\"",
                lossy(alias),
                lossy(&name),
                lossy(fname)
            );
            if free_alias(req, id) {
                return Err(None);
            }
            return Err(Some(message));
        }
        remember(req, id, Some(alias));
        return Ok(id);
    }
    if fname.is_empty() {
        return Err(None);
    }
    if let Some(id) = table.by_fname(fname) {
        if let Some(alias) = alias {
            let Some(a) = table.get(id) else {
                return Err(None);
            };
            if !a.is_temporary_alias && a.alias.as_deref() != Some(alias) {
                return Err(Some(format!(
                    "alias \"{}\" is already used for archive \"{}\" cannot be overloaded with \"{}\"",
                    lossy(alias),
                    lossy(&a.fname),
                    lossy(fname)
                )));
            }
            if let Some(current) = a.alias.clone()
                && table.alias_map.contains(&current)
            {
                table.alias_map.remove(&current);
            }
            table.alias_map.add(alias, id).ok();
        }
        remember(req, id, None);
        return Ok(id);
    }
    if let Some(id) = table.by_alias(fname) {
        remember(req, id, None);
        return Ok(id);
    }
    let Some(real) = expand_filepath(req, fname) else {
        return Err(None);
    };
    let table = archives(req);
    if let Some(id) = table.by_fname(&real) {
        if let Some(alias) = alias {
            table.alias_map.add(alias, id).ok();
        }
        remember(req, id, None);
        return Ok(id);
    }
    Err(None)
}

/// The last-found cache set to an archive: its name, and the alias
/// looked up or else its own.
fn remember(req: &mut Request, id: ArchiveId, alias: Option<&[u8]>) {
    let table = archives(req);
    let Some(archive) = table.get(id) else {
        return;
    };
    table.last = Some(Last {
        id,
        name: archive.fname.clone(),
        alias: alias.map(<[u8]>::to_vec).or_else(|| archive.alias.clone()),
    });
}

/// `expand_filepath`: the path made absolute against the working
/// directory, `.` and `..` taken out and links resolved where the file is
/// there.
pub(crate) fn expand_filepath(req: &mut Request, path: &[u8]) -> Option<Vec<u8>> {
    let cwd = req
        .environment()
        .cwd()
        .into_os_string()
        .into_encoded_bytes();
    req.environment_mut()
        .virtual_file_ex(&cwd, path, php_ext::request::Resolve::FilePath)
}

/// A byte string for a message.
pub(crate) fn lossy(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

/// A string of the request's from bytes, as a value.
pub(crate) fn string(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

// ---- reading and writing a file whole through the streams layer

/// `php_stream_open_wrapper(path, "rb", ...)` and the stream copied to
/// memory, then closed; `None` where it would not open.
pub(crate) fn read_file(
    req: &mut Request,
    path: &[u8],
    warn: bool,
) -> Result<Option<Vec<u8>>, Thrown> {
    read_range(req, path, warn, 0, None)
}

/// `php_stream_open_wrapper(path, "rb", STREAM_MUST_SEEK, NULL)`.
pub(crate) fn open_rb(
    req: &mut Request,
    path: &[u8],
    warn: bool,
) -> Result<Option<Owner<Stream>>, Thrown> {
    let opening = Opening {
        warn,
        must_seek: true,
        ..Opening::default()
    };
    req.open_stream_with(path, b"rb", opening)
}

/// [`read_at`] of the file at `path`, opened for the one read.
pub(crate) fn read_range(
    req: &mut Request,
    path: &[u8],
    warn: bool,
    at: i64,
    len: Option<usize>,
) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(stream) = open_rb(req, path, warn)? else {
        return Ok(None);
    };
    let bytes = read_at(req, *stream, at, len);
    stream.free(req);
    Ok(Some(bytes?))
}

/// `len` bytes from `at`, or the rest, of what a flush and `getStub()`
/// read the old archive from: `phar->fp`, or for an archive new to this
/// request the file opened for the read; `None` where it will not open.
pub(crate) fn read_old(
    req: &mut Request,
    id: ArchiveId,
    at: i64,
    len: Option<usize>,
) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(a) = archives(req).get(id) else {
        return Ok(None);
    };
    let fp = a.fp.as_ref().filter(|_| !a.is_brandnew).map(Fp::efp);
    let fname = a.fname.clone();
    match fp {
        Some(Efp::Stream(stream)) => read_at(req, stream, at, len).map(Some),
        Some(Efp::Shared(bytes)) => {
            let from = usize::try_from(at).unwrap_or(usize::MAX).min(bytes.len());
            let to = len.map_or(bytes.len(), |len| from.saturating_add(len).min(bytes.len()));
            Ok(Some(bytes[from..to].to_vec()))
        }
        _ => read_range(req, &fname, false, at, len),
    }
}

/// The rest of a stream as bytes.
pub(crate) fn copy_all(req: &mut Request, stream: Stream) -> Result<Vec<u8>, Thrown> {
    let s = stream.copy_to_mem(req, None)?;
    let bytes = s.as_bytes().to_vec();
    s.give_up(req);
    Ok(bytes)
}

/// `len` bytes of a stream from `at`, fewer at its end, or the rest of
/// it.
pub(crate) fn read_at(
    req: &mut Request,
    stream: Stream,
    at: i64,
    len: Option<usize>,
) -> Result<Vec<u8>, Thrown> {
    stream.seek(req, at, Whence::Set);
    let s = stream.copy_to_mem(req, len)?;
    let bytes = s.as_bytes().to_vec();
    s.give_up(req);
    Ok(bytes)
}

/// `bytes` through the stream filter `name`, as phar appends one to a
/// temporary stream's write chain, copies into it, flushes and removes
/// it: what the filter made, or `None` where the filter could not be
/// made.
pub(crate) fn filter_bytes(
    req: &mut Request,
    name: &[u8],
    params: Option<&Value>,
    bytes: &[u8],
) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(temp) = req.open_stream(b"php://memory", b"w+b", false)? else {
        return Ok(None);
    };
    let filter = temp.add_filter(req, name, params, 2, true);
    let out = match filter {
        Ok(Some(filter)) => {
            let mut at = 0;
            while at < bytes.len() {
                let end = (at + 8192).min(bytes.len());
                if temp.write(req, &bytes[at..end]).is_none() {
                    break;
                }
                at = end;
            }
            let removed = req.remove_filter(*filter);
            filter.give_up(req);
            match removed {
                Ok(_) => {
                    temp.seek(req, 0, Whence::Set);
                    copy_all(req, *temp).map(Some)
                }
                Err(thrown) => Err(thrown),
            }
        }
        Ok(None) => Ok(None),
        Err(thrown) => Err(thrown),
    };
    temp.free(req);
    out
}
