//! `util.c`'s entries: finding one by its path (`phar_get_entry_info_dir`),
//! mounting an outside file (`phar_mount_entry`), opening one to read or
//! write (`phar_get_entry_data`, `phar_get_or_create_entry_data`), making
//! its contents readable where it is compressed (`phar_open_entry_fp`) and
//! checking its CRC (`phar_postprocess_file`), and following a tar link
//! (`phar_get_link_source`).

use std::sync::Arc;

use php_ext::raising::Thrown;
use php_ext::request::{OwnedStream as _, Request, Stream, Streams as _};

use crate::archive::{
    self, ArchiveId, Content, ENT_PERM_DEF_FILE, Entry, TAR_FILE, archives, lossy,
};
use crate::open::{Failed, readonly};
use crate::path;

/// Which entry an open entry stream names: a manifest row by its slot, or an
/// entry of its own that is in no manifest, a directory made up for a
/// path or the stub an include of the bare archive reads.
pub(crate) enum Target {
    Slot(usize),
    Temp(Box<Entry>),
}

/// `phar_entry_data`: an entry open for a stream.
pub(crate) struct EntryData {
    pub(crate) archive: ArchiveId,
    pub(crate) target: Target,
    /// `idata->fp`: the stream the contents are read from, taken when the
    /// entry opened, the end of its links'; `None` for a directory.
    pub(crate) fp: Option<Efp>,
    /// Where the stream stands, from the start of the contents.
    pub(crate) position: i64,
    /// Where the contents start in the bytes they are read from.
    pub(crate) zero: i64,
}

/// What `phar_get_entry_info_dir` found: a manifest row, or a directory
/// that is only a prefix of rows.
pub(crate) enum Found {
    Slot(usize),
    TempDir(Box<Entry>),
}

/// `dir` of `phar_get_entry_info_dir`: `0`, files only; `1`, directories
/// too, a prefix of entries included; `2`, directories only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dir {
    No,
    Allow,
    Only,
}

/// `phar_get_entry_info_dir`: the entry at `path` in the archive, a
/// directory made up where `dir` allows and entries stand under it, or a
/// file of a mounted directory mounted where it is looked for; the
/// message where one is due, `None` for a path that is simply absent.
#[allow(clippy::too_many_lines)] // php's one function, its refusals in its order
pub(crate) fn find(
    req: &mut Request,
    id: ArchiveId,
    path: &[u8],
    dir: Dir,
    security: bool,
) -> Result<Result<Found, Failed>, Thrown> {
    let is_dir = path.last() == Some(&b'/');
    if security && path::is_magic(path) {
        return Ok(Err(Some(
            "phar error: cannot directly access magic \".phar\" directory or files within it"
                .into(),
        )));
    }
    if path.is_empty() && dir == Dir::No {
        return Ok(Err(Some(
            "phar error: invalid path \"\" must not be empty".into(),
        )));
    }
    let path = match path::check(path) {
        Ok(checked) => checked,
        Err(e) => {
            return Ok(Err(Some(format!(
                "phar error: invalid path \"{}\" contains {}",
                lossy(path),
                e.words()
            ))));
        }
    };
    let mut path = path.as_slice();
    if is_dir {
        if path.len() <= 1 {
            return Ok(Err(None));
        }
        path = &path[..path.len() - 1];
    }
    let Some(archive) = archives(req).get(id) else {
        return Ok(Err(None));
    };
    if let Some(slot) = archive.manifest.slot(path) {
        let Some(entry) = archive.manifest.at(slot) else {
            return Ok(Err(None));
        };
        if entry.is_deleted {
            return Ok(Err(None));
        }
        if entry.is_dir && dir == Dir::No {
            return Ok(Err(Some(format!(
                "phar error: path \"{}\" is a directory",
                lossy(path)
            ))));
        }
        if !entry.is_dir && dir == Dir::Only {
            return Ok(Err(Some(format!(
                "phar error: path \"{}\" exists and is a not a directory",
                lossy(path)
            ))));
        }
        return Ok(Ok(Found::Slot(slot)));
    }
    if dir != Dir::No && archive.virtual_dirs.contains(path) {
        let mut entry = Entry::new(path);
        entry.is_temp_dir = true;
        entry.is_dir = true;
        return Ok(Ok(Found::TempDir(Box::new(entry))));
    }
    for mounted in archive.mounted_dirs.keys() {
        if mounted.len() >= path.len() || !path.starts_with(&mounted) {
            continue;
        }
        let archive = archives(req).get(id);
        let Some(tmp) = archive
            .and_then(|a| a.manifest.get(&mounted))
            .filter(|e| e.is_mounted)
            .and_then(|e| e.tmp.clone())
        else {
            return Ok(Err(Some(format!(
                "phar internal error: mounted path \"{}\" could not be retrieved from manifest",
                lossy(&mounted)
            ))));
        };
        let mut test = tmp;
        test.extend_from_slice(&path[mounted.len()..]);
        let Some(sb) = req.url_stat(&test, false, false)?.found() else {
            return Ok(Err(None));
        };
        let is_directory = sb[2] & 0o040_000 != 0;
        if is_directory && dir == Dir::No {
            return Ok(Err(Some(format!(
                "phar error: path \"{}\" is a directory",
                lossy(path)
            ))));
        }
        if !is_directory && dir != Dir::No {
            return Ok(Err(Some(format!(
                "phar error: path \"{}\" exists and is a not a directory",
                lossy(path)
            ))));
        }
        if !mount(req, id, &test, path)? {
            return Ok(Err(Some(format!(
                "phar error: path \"{}\" exists as file \"{}\" and could not be mounted",
                lossy(path),
                lossy(&test)
            ))));
        }
        return Ok(
            match archives(req).get(id).and_then(|a| a.manifest.slot(path)) {
                Some(slot) => Ok(Found::Slot(slot)),
                None => Err(Some(format!(
                    "phar error: path \"{}\" exists as file \"{}\" and could not be retrieved after being mounted",
                    lossy(path),
                    lossy(&test)
                ))),
            },
        );
    }
    Ok(Err(None))
}

/// `phar_mount_entry`: the outside file or directory `filename` mounted
/// at `path` in the archive; `false` where it could not be.
pub(crate) fn mount(
    req: &mut Request,
    id: ArchiveId,
    filename: &[u8],
    path: &[u8],
) -> Result<bool, Thrown> {
    let Ok(path) = path::check(path) else {
        return Ok(false);
    };
    if path::is_magic(&path) {
        return Ok(false);
    }
    let is_phar = filename.len() > 7 && filename.starts_with(b"phar://");
    let tmp = if is_phar {
        filename.to_vec()
    } else {
        archive::expand_filepath(req, filename).unwrap_or_else(|| filename.to_vec())
    };
    // "only check openbasedir for files, not for phar streams"
    if !is_phar
        && req
            .environment_mut()
            .check_open_basedir(&tmp, true)
            .is_err()
    {
        return Ok(false);
    }
    let Some(sb) = req.url_stat(&tmp, false, false)?.found() else {
        return Ok(false);
    };
    let mut entry = Entry::new(&path);
    entry.is_mounted = true;
    entry.is_crc_checked = true;
    entry.content = Content::Tmp(None);
    entry.tmp = Some(tmp);
    entry.flags = u32::try_from(sb[2]).unwrap_or(0);
    let Some(archive) = archives(req).get_mut(id) else {
        return Ok(false);
    };
    if sb[2] & 0o040_000 != 0 {
        entry.is_dir = true;
        if archive.mounted_dirs.add(&path, ()).is_err() {
            return Ok(false);
        }
    } else {
        let size = u32::try_from(sb[7]).unwrap_or(u32::MAX);
        entry.uncompressed_filesize = size;
        entry.compressed_filesize = size;
    }
    Ok(archive.manifest.add(&path, entry).is_ok())
}

/// `phar_follow_one_link`: the entry a link names, looked up by the link
/// as written and then by `phar_get_link_location`'s path, which for a
/// relative link in a directory also cuts the linking entry's own name
/// to that directory, as php's does.
fn follow_one(req: &mut Request, id: ArchiveId, slot: usize) -> Option<usize> {
    let archive = archives(req).get_mut(id)?;
    let entry = archive.manifest.at_mut(slot)?;
    let link = entry.link.clone()?;
    let location = if let Some(abs) = link.strip_prefix(b"/") {
        abs.to_vec()
    } else if let Some(cut) = entry.filename.iter().rposition(|b| *b == b'/') {
        entry.filename.truncate(cut);
        [entry.filename.as_slice(), b"/", link.as_slice()].concat()
    } else {
        link.clone()
    };
    archive
        .manifest
        .slot(&link)
        .or_else(|| archive.manifest.slot(&location))
}

/// `phar_get_link_source`: the entry a chain of links ends at, the entry
/// itself where it is none, `None` for a chain that loops or breaks.
pub(crate) fn link_source(req: &mut Request, id: ArchiveId, slot: usize) -> Option<usize> {
    let has_link = |req: &mut Request, s: usize| {
        archives(req)
            .get(id)
            .and_then(|a| a.manifest.at(s))
            .is_some_and(|e| e.link.is_some())
    };
    if !has_link(req, slot) {
        return Some(slot);
    }
    let (mut tortoise, mut hare) = (slot, slot);
    loop {
        hare = follow_one(req, id, hare)?;
        if !has_link(req, hare) {
            return Some(hare);
        }
        hare = follow_one(req, id, hare)?;
        if !has_link(req, hare) {
            return Some(hare);
        }
        tortoise = follow_one(req, id, tortoise)?;
        if tortoise == hare {
            return None;
        }
    }
}

/// `phar_open_archive_fp`: the archive's file read again where its bytes
/// were let go; `false` where it will not open.
pub(crate) fn open_archive_fp(req: &mut Request, id: ArchiveId) -> Result<bool, Thrown> {
    let Some(archive) = archives(req).get(id) else {
        return Ok(false);
    };
    if archive.fp.is_some() {
        return Ok(true);
    }
    let fname = archive.fname.clone();
    if req
        .environment_mut()
        .check_open_basedir(&fname, true)
        .is_err()
    {
        return Ok(false);
    }
    let bytes = archive::read_file(req, &fname, false)?;
    let Some(archive) = archives(req).get_mut(id) else {
        return Ok(false);
    };
    archive.fp = bytes.map(|bytes| archive::Fp {
        bytes: Arc::new(bytes),
        through: archive::Through::File,
    });
    Ok(archive.fp.is_some())
}

/// The stream an entry's contents are read from, `phar_get_efp`: the
/// archive's bytes or its decompressed buffer, shared, which an entry
/// stream keeps as php's keeps `idata->fp`; the entry's own buffer, read
/// where it stands; or a mounted file's stream.
#[derive(Clone)]
pub(crate) enum Efp {
    Shared(Arc<Vec<u8>>),
    Own,
    Mounted(Stream),
}

/// `phar_get_efp`: the stream the entry at `target` is read from, the
/// archive's file read again and a mounted file opened where they must
/// be; `None` where there is none.
pub(crate) fn efp(
    req: &mut Request,
    id: ArchiveId,
    target: &Target,
) -> Result<Option<Efp>, Thrown> {
    let kind = match target {
        Target::Temp(entry) => kind_of(&entry.content),
        Target::Slot(slot) => match archives(req).get(id).and_then(|a| a.manifest.at(*slot)) {
            Some(entry) => kind_of(&entry.content),
            None => return Ok(None),
        },
    };
    Ok(match kind {
        Kind::Fp => {
            open_archive_fp(req, id)?;
            archives(req)
                .get(id)
                .and_then(|a| a.fp.as_ref())
                .map(|fp| Efp::Shared(Arc::clone(&fp.bytes)))
        }
        Kind::Ufp => archives(req)
            .get(id)
            .and_then(|a| a.ufp.clone())
            .map(Efp::Shared),
        Kind::Mod => Some(Efp::Own),
        Kind::Tmp => match target {
            Target::Slot(slot) => tmp_stream(req, id, *slot)?.map(Efp::Mounted),
            Target::Temp(_) => None,
        },
    })
}

/// `len` bytes of `efp` from `from`, fewer at its end; the entry's own
/// buffer read at `target`.
pub(crate) fn read_efp(
    req: &mut Request,
    id: ArchiveId,
    target: &Target,
    efp: &Efp,
    from: i64,
    len: usize,
) -> Result<Vec<u8>, Thrown> {
    let at = usize::try_from(from).unwrap_or(usize::MAX);
    let slice = |bytes: &[u8]| {
        bytes
            .get(at..)
            .map_or_else(Vec::new, |b| b[..len.min(b.len())].to_vec())
    };
    Ok(match efp {
        Efp::Shared(bytes) => slice(bytes),
        Efp::Own => {
            let content = match target {
                Target::Temp(entry) => Some(&entry.content),
                Target::Slot(slot) => archives(req)
                    .get(id)
                    .and_then(|a| a.manifest.at(*slot))
                    .map(|e| &e.content),
            };
            match content {
                Some(Content::Mod(buf)) => slice(buf),
                _ => Vec::new(),
            }
        }
        Efp::Mounted(stream) => archive::read_at(req, *stream, from, len)?,
    })
}

/// The bytes `[from, from + len)` of the stream an entry's contents are
/// read from now, [`efp`] then [`read_efp`].
pub(crate) fn efp_read(
    req: &mut Request,
    id: ArchiveId,
    target: &Target,
    from: i64,
    len: usize,
) -> Result<Vec<u8>, Thrown> {
    match efp(req, id, target)? {
        Some(efp) => read_efp(req, id, target, &efp, from, len),
        None => Ok(Vec::new()),
    }
}

/// The four stream kinds, without what they hold.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fp,
    Ufp,
    Mod,
    Tmp,
}

fn kind_of(content: &Content) -> Kind {
    match content {
        Content::Fp => Kind::Fp,
        Content::Ufp => Kind::Ufp,
        Content::Mod(_) => Kind::Mod,
        Content::Tmp(_) => Kind::Tmp,
    }
}

/// A mounted entry's stream, `php_stream_open_wrapper(entry->tmp, "rb",
/// STREAM_MUST_SEEK)` the first time.
fn tmp_stream(req: &mut Request, id: ArchiveId, slot: usize) -> Result<Option<Stream>, Thrown> {
    let Some(entry) = archives(req).get(id).and_then(|a| a.manifest.at(slot)) else {
        return Ok(None);
    };
    if let Content::Tmp(Some(stream)) = &entry.content {
        return Ok(Some(**stream));
    }
    let Some(tmp) = entry.tmp.clone() else {
        return Ok(None);
    };
    // The entry keeps the stream, and its owner with it.
    let Some(stream) = req.open_stream_with(&tmp, b"rb", php_ext::request::Opening::default())?
    else {
        return Ok(None);
    };
    let view = *stream;
    let Some(entry) = archives(req)
        .get_mut(id)
        .and_then(|a| a.manifest.at_mut(slot))
    else {
        stream.free(req);
        return Ok(None);
    };
    entry.content = Content::Tmp(Some(stream));
    Ok(Some(view))
}

/// A whole entry's contents, as its stream reads them.
pub(crate) fn contents(req: &mut Request, id: ArchiveId, slot: usize) -> Result<Vec<u8>, Thrown> {
    let Some(entry) = archives(req).get(id).and_then(|a| a.manifest.at(slot)) else {
        return Ok(Vec::new());
    };
    let (offset, size) = (entry.offset, entry.uncompressed_filesize as usize);
    efp_read(req, id, &Target::Slot(slot), offset, size)
}

/// `phar_open_entry_fp`: an entry's contents made readable, a compressed
/// one decompressed into the archive's buffer through its stream filter,
/// each checked against its CRC; the message where one of them fails.
pub(crate) fn open_entry_fp(
    req: &mut Request,
    id: ArchiveId,
    slot: usize,
    follow: bool,
) -> Result<Result<(), String>, Thrown> {
    let slot = if follow {
        link_source(req, id, slot).unwrap_or(slot)
    } else {
        slot
    };
    let Some(archive) = archives(req).get(id) else {
        return Ok(Ok(()));
    };
    let fname = archive.fname.clone();
    let Some(entry) = archive.manifest.at(slot) else {
        return Ok(Ok(()));
    };
    if entry.is_modified {
        return Ok(Ok(()));
    }
    if let Content::Tmp(_) = entry.content {
        tmp_stream(req, id, slot)?;
        return Ok(Ok(()));
    }
    if !matches!(entry.content, Content::Fp) {
        return Ok(Ok(()));
    }
    let name = entry.filename.clone();
    let compressed = entry.flags & archive::ENT_COMPRESSION_MASK;
    let plain = (entry.old_flags != 0 && entry.old_flags & archive::ENT_COMPRESSION_MASK == 0)
        || compressed == 0;
    if !open_archive_fp(req, id)? {
        return Ok(Err(format!(
            "phar error: Cannot open phar archive \"{}\" for reading",
            lossy(&fname)
        )));
    }
    if plain {
        return Ok(Ok(()));
    }
    let filter: &[u8] = if compressed == archive::ENT_COMPRESSED_GZ {
        b"zlib.inflate"
    } else {
        b"bzip2.decompress"
    };
    let Some(entry) = archives(req).get(id).and_then(|a| a.manifest.at(slot)) else {
        return Ok(Ok(()));
    };
    let (offset, csize, usize_) = (
        entry.offset,
        entry.compressed_filesize as usize,
        entry.uncompressed_filesize,
    );
    let raw = if usize_ != 0 {
        efp_read(req, id, &Target::Slot(slot), offset, csize)?
    } else {
        Vec::new()
    };
    let Some(inflated) = archive::filter_bytes(req, filter, None, &raw)? else {
        return Ok(Err(format!(
            "phar error: unable to read phar \"{}\" (cannot create {} filter while decompressing file \"{}\")",
            lossy(&fname),
            lossy(filter),
            lossy(&name)
        )));
    };
    if raw.len() < csize && usize_ != 0 || inflated.len() != usize_ as usize {
        return Ok(Err(format!(
            "phar error: internal corruption of phar \"{}\" (actual filesize mismatch on file \"{}\")",
            lossy(&fname),
            lossy(&name)
        )));
    }
    let Some(archive) = archives(req).get_mut(id) else {
        return Ok(Ok(()));
    };
    let ufp = Arc::make_mut(archive.ufp.get_or_insert_with(Default::default));
    let loc = ufp.len();
    ufp.extend_from_slice(&inflated);
    if let Some(entry) = archive.manifest.at_mut(slot) {
        entry.old_flags = entry.flags;
        entry.content = Content::Ufp;
        entry.offset = i64::try_from(loc).unwrap_or(0);
    }
    check_crc(req, id, slot)
}

/// `phar_postprocess_file`'s CRC half: the entry's contents against the
/// CRC it records, and the entry marked checked where they agree.
pub(crate) fn check_crc(
    req: &mut Request,
    id: ArchiveId,
    slot: usize,
) -> Result<Result<(), String>, Thrown> {
    let Some(archive) = archives(req).get(id) else {
        return Ok(Ok(()));
    };
    let fname = archive.fname.clone();
    let Some(entry) = archive.manifest.at(slot) else {
        return Ok(Ok(()));
    };
    let (expected, name) = (entry.crc32, entry.filename.clone());
    let data = contents(req, id, slot)?;
    let size = archives(req)
        .get(id)
        .and_then(|a| a.manifest.at(slot))
        .map_or(0, |e| e.uncompressed_filesize as usize);
    let crc = !php_ext_standard::crc32::update(u32::MAX, &data);
    if data.len() == size && crc == expected {
        if let Some(entry) = archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.at_mut(slot))
        {
            entry.is_crc_checked = true;
        }
        return Ok(Ok(()));
    }
    Ok(Err(format!(
        "phar error: internal corruption of phar \"{}\" (crc32 mismatch on file \"{}\")",
        lossy(&fname),
        lossy(&name)
    )))
}

/// `phar_get_entry_data`: an existing entry opened for `mode`, refused
/// where writes are off, where a write meets an open reader or a read
/// meets a pending write; `Ok(None)` where it is absent and may be
/// created.
#[allow(clippy::too_many_lines)] // php's one function, its refusals in its order
pub(crate) fn get_entry_data(
    req: &mut Request,
    fname: &[u8],
    path: &[u8],
    mode: &[u8],
    allow_dir: Dir,
    security: bool,
) -> Result<Result<Option<EntryData>, Failed>, Thrown> {
    let for_write = mode.first() != Some(&b'r') || mode.get(1) == Some(&b'+');
    let for_append = mode.first() == Some(&b'a');
    let for_create = mode.first() != Some(&b'r');
    let for_trunc = mode.first() == Some(&b'w');
    let id = match archive::get_archive(req, fname, None) {
        Ok(id) => id,
        Err(e) => return Ok(Err(e)),
    };
    let ro = readonly(req);
    let is_data = archives(req).get(id).is_some_and(|a| a.is_data);
    if for_write && ro && !is_data {
        return Ok(Err(Some(format!(
            "phar error: file \"{}\" in phar \"{}\" cannot be opened for writing, disabled by ini setting",
            lossy(path),
            lossy(fname)
        ))));
    }
    if path.is_empty() {
        return Ok(Err(Some(format!(
            "phar error: file \"\" in phar \"{}\" must not be empty",
            lossy(fname)
        ))));
    }
    let may_create = for_create && (!ro || is_data);
    let found = find(req, id, path, allow_dir, security)?;
    let found = match found {
        Ok(found) => found,
        Err(error) => {
            if may_create {
                return Ok(Ok(None));
            }
            return Ok(Err(error));
        }
    };
    let slot = match found {
        Found::Slot(slot) => slot,
        Found::TempDir(entry) => {
            if let Some(a) = archives(req).get_mut(id) {
                a.refcount += 1;
            }
            return Ok(Ok(Some(EntryData {
                archive: id,
                target: Target::Temp(entry),
                fp: None,
                position: 0,
                zero: 0,
            })));
        }
    };
    let is_tar = archives(req).get(id).is_some_and(archive::Archive::is_tar);
    let Some(entry) = archives(req)
        .get_mut(id)
        .and_then(|a| a.manifest.at_mut(slot))
    else {
        return Ok(Err(None));
    };
    if entry.is_modified && !for_write {
        return Ok(Err(Some(format!(
            "phar error: file \"{}\" in phar \"{}\" cannot be opened for reading, writable file pointers are open",
            lossy(path),
            lossy(fname)
        ))));
    }
    if entry.fp_refcount != 0 && for_write {
        return Ok(Err(Some(format!(
            "phar error: file \"{}\" in phar \"{}\" cannot be opened for writing, readable file pointers are open",
            lossy(path),
            lossy(fname)
        ))));
    }
    if entry.is_deleted {
        if !for_create {
            return Ok(Err(None));
        }
        entry.is_deleted = false;
    }
    if entry.is_dir {
        entry.fp_refcount += 1;
        if let Some(a) = archives(req).get_mut(id) {
            a.refcount += 1;
        }
        return Ok(Ok(Some(EntryData {
            archive: id,
            target: Target::Slot(slot),
            fp: None,
            position: 0,
            zero: 0,
        })));
    }
    if let Content::Mod(_) = entry.content {
        if for_trunc {
            create_writeable(req, id, slot);
        } else if for_append {
            // `phar_seek_efp(entry, 0, SEEK_END, 0, 0)`: nothing to keep,
            // a stream's position is its own.
        }
    } else if for_write {
        if entry.link.is_some() {
            entry.link = None;
            entry.tar_type = if is_tar { TAR_FILE } else { 0 };
        }
        if for_trunc {
            create_writeable(req, id, slot);
        } else if let Err(error) = separate(req, id, slot)? {
            return Ok(Err(Some(error)));
        }
    } else if let Err(error) = open_entry_fp(req, id, slot, true)? {
        return Ok(Err(Some(error)));
    }
    let source = link_source(req, id, slot);
    if archives(req)
        .get(id)
        .and_then(|a| a.manifest.at(slot))
        .is_some_and(|e| e.link.is_some())
        && source.is_none()
    {
        return Ok(Err(None));
    }
    let source = source.unwrap_or(slot);
    let zero = archives(req)
        .get(id)
        .and_then(|a| a.manifest.at(source))
        .map_or(0, |e| e.offset);
    let fp = efp(req, id, &Target::Slot(source))?;
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(Err(None));
    };
    a.refcount += 1;
    if let Some(entry) = a.manifest.at_mut(slot) {
        entry.fp_refcount += 1;
    }
    Ok(Ok(Some(EntryData {
        archive: id,
        target: Target::Slot(slot),
        fp,
        position: 0,
        zero,
    })))
}

/// `phar_create_writeable_entry`: the entry's contents emptied into a
/// buffer of its own, its sizes, CRC and flags reset.
fn create_writeable(req: &mut Request, id: ArchiveId, slot: usize) {
    let Some(archive) = archives(req).get_mut(id) else {
        return;
    };
    archive.is_modified = true;
    let is_tar = archive.is_tar();
    let Some(entry) = archive.manifest.at_mut(slot) else {
        return;
    };
    if !matches!(entry.content, Content::Mod(_)) && entry.link.is_some() {
        entry.link = None;
        entry.tar_type = if is_tar { TAR_FILE } else { 0 };
    }
    let old = std::mem::replace(&mut entry.content, Content::Mod(Vec::new()));
    entry.old_flags = entry.flags;
    entry.is_modified = true;
    entry.uncompressed_filesize = 0;
    entry.compressed_filesize = 0;
    entry.crc32 = 0;
    entry.flags = ENT_PERM_DEF_FILE;
    entry.offset = 0;
    if let Content::Tmp(Some(stream)) = old {
        stream.free(req);
    }
}

/// `phar_separate_entry_fp`: the entry's contents copied into a buffer of
/// its own, from the end of its links, for a write that keeps them.
fn separate(req: &mut Request, id: ArchiveId, slot: usize) -> Result<Result<(), String>, Thrown> {
    if let Err(error) = open_entry_fp(req, id, slot, true)? {
        return Ok(Err(error));
    }
    if matches!(
        archives(req)
            .get(id)
            .and_then(|a| a.manifest.at(slot))
            .map(|e| &e.content),
        Some(Content::Mod(_))
    ) {
        return Ok(Ok(()));
    }
    let source = link_source(req, id, slot).unwrap_or(slot);
    let data = contents(req, id, source)?;
    let Some(archive) = archives(req).get_mut(id) else {
        return Ok(Ok(()));
    };
    let is_tar = archive.is_tar();
    if let Some(entry) = archive.manifest.at_mut(slot) {
        if entry.link.is_some() {
            entry.link = None;
            entry.tar_type = if is_tar { TAR_FILE } else { 0 };
        }
        entry.offset = 0;
        let old = std::mem::replace(&mut entry.content, Content::Mod(data));
        entry.is_modified = true;
        if let Content::Tmp(Some(stream)) = old {
            stream.free(req);
        }
    }
    Ok(Ok(()))
}

/// `phar_entry_delref`: an open entry closed, its reader count and the
/// archive's references given back.
#[allow(clippy::needless_pass_by_value)] // the handle is spent, as php frees `idata`
pub(crate) fn delref(req: &mut Request, data: EntryData) {
    if let Target::Slot(slot) = data.target
        && let Some(entry) = archives(req)
            .get_mut(data.archive)
            .and_then(|a| a.manifest.at_mut(slot))
    {
        entry.fp_refcount = (entry.fp_refcount - 1).max(0);
    }
    archive::delref(req, data.archive);
}
