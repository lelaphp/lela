//! `phar_convert_to_other` and `phar_rename_archive`: an archive copied
//! whole into a new one of another format or compression, named by the
//! old name with the new extension, written, and opened as a `Phar` or a
//! `PharData`; and the argument rules of the four methods that ask for
//! it.

use std::sync::Arc;

use php_ext::call::instantiate;
use php_ext::raising::{Thrown, throw_exception_ex};
use php_ext::request::{Request, Streams as _};
use php_ext::values::{Owner, PhpObject};

use crate::archive::{
    self, Archive, ArchiveId, Content, ENT_COMPRESSION_MASK, FILE_COMPRESSED_BZ2,
    FILE_COMPRESSED_GZ, FILE_COMPRESSION_MASK, Format, TAR_DIR, TAR_FILE, archives, lossy, string,
};
use crate::entry::{self, Target};
use crate::object::class_named;
use crate::open::{has_bz2, has_zlib};
use crate::path::{self, Create, Exec};
use crate::write;

/// The extensions a name loses before the new one is put on, longest
/// first.
const PHAR_EXTENSIONS: [&[u8]; 12] = [
    b".phar.tar.bz2",
    b".phar.tar.gz",
    b".phar.php",
    b".phar.bz2",
    b".phar.zip",
    b".phar.tar",
    b".phar.gz",
    b".tar.bz2",
    b".tar.gz",
    b".phar",
    b".tar",
    b".zip",
];

/// `Phar::convertToExecutable()` and `Phar::convertToData()` from their
/// arguments: the format, the same one where none is given, and the
/// whole-archive compression, the archive's own where none is given.
pub(crate) fn convert(
    req: &mut Request,
    id: ArchiveId,
    data: bool,
    format: Option<i64>,
    compression: Option<i64>,
    ext: Option<&[u8]>,
) -> Result<Option<Owner<PhpObject>>, Thrown> {
    let Some(a) = archives(req).get(id) else {
        return Ok(None);
    };
    let (current, own_flags) = (a.format, a.flags & FILE_COMPRESSION_MASK);
    let format = match format {
        None | Some(9_021_976 | -1) => match current {
            Format::Phar if data => {
                return Err(throw_exception_ex(
                    req,
                    "UnexpectedValueException",
                    "Cannot write out data phar archive, use Phar::TAR or Phar::ZIP",
                ));
            }
            format => format,
        },
        Some(1) if data => {
            return Err(throw_exception_ex(
                req,
                "UnexpectedValueException",
                "Cannot write out data phar archive, use Phar::TAR or Phar::ZIP",
            ));
        }
        Some(1) => Format::Phar,
        Some(2) => Format::Tar,
        Some(3) => Format::Zip,
        Some(_) => {
            let message = if data {
                "Unknown file format specified, please pass one of Phar::TAR or Phar::ZIP"
            } else {
                "Unknown file format specified, please pass one of Phar::PHAR, Phar::TAR or Phar::ZIP"
            };
            return Err(throw_exception_ex(req, "BadMethodCallException", message));
        }
    };
    let bad =
        |req: &mut Request, m: &str| Err(throw_exception_ex(req, "BadMethodCallException", m));
    let flags = match compression {
        None | Some(9_021_976) => own_flags,
        Some(0) => 0,
        Some(0x1000) if format == Format::Zip => {
            return bad(
                req,
                "Cannot compress entire archive with gzip, zip archives do not support whole-archive compression",
            );
        }
        Some(0x1000) if !has_zlib(req) => {
            return bad(
                req,
                "Cannot compress entire archive with gzip, enable ext/zlib in php.ini",
            );
        }
        Some(0x1000) => FILE_COMPRESSED_GZ,
        Some(0x2000) if format == Format::Zip => {
            return bad(
                req,
                "Cannot compress entire archive with bz2, zip archives do not support whole-archive compression",
            );
        }
        Some(0x2000) if !has_bz2(req) => {
            return bad(
                req,
                "Cannot compress entire archive with bz2, enable ext/bz2 in php.ini",
            );
        }
        Some(0x2000) => FILE_COMPRESSED_BZ2,
        Some(_) => {
            return bad(
                req,
                "Unknown compression specified, please pass one of Phar::GZ or Phar::BZ2",
            );
        }
    };
    let saved = archives(req)
        .get_mut(id)
        .map(|a| std::mem::replace(&mut a.is_data, data));
    let converted = to_other(req, id, format, ext, flags);
    if let (Some(saved), Some(a)) = (saved, archives(req).get_mut(id)) {
        a.is_data = saved;
    }
    converted
}

/// `phar_convert_to_other`: every entry's uncompressed contents copied
/// into the new archive's bytes, each entry to be compressed by its own
/// flags as it is written, then [`rename`].
pub(crate) fn to_other(
    req: &mut Request,
    id: ArchiveId,
    format: Format,
    ext: Option<&[u8]>,
    flags: u32,
) -> Result<Option<Owner<PhpObject>>, Thrown> {
    archives(req).last = None;
    let Some(source) = archives(req).get_mut(id) else {
        return Ok(None);
    };
    let mut phar = Archive::new(&source.fname.clone(), format);
    phar.flags = flags;
    phar.is_data = source.is_data && format != Format::Phar;
    phar.is_temporary_alias = source.is_temporary_alias;
    phar.alias.clone_from(&source.alias);
    let mut meta = std::mem::take(&mut source.metadata);
    phar.metadata = meta.copied(req);
    match archives(req).get_mut(id) {
        Some(source) => source.metadata = meta,
        None => meta.free(req),
    }
    let fname = phar.fname.clone();
    let mut fp: Vec<u8> = Vec::new();
    let slots = archives(req)
        .get(id)
        .map(|a| a.manifest.slots())
        .unwrap_or_default();
    for slot in slots {
        let Some(e) = archives(req).get(id).and_then(|a| a.manifest.at(slot)) else {
            continue;
        };
        let mut copy = e.shallow(&e.filename.clone());
        let (is_dir, plain) = (e.is_dir, e.link.is_none() && e.tmp.is_none());
        let name = lossy(&e.filename).into_owned();
        let mut meta = archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.at_mut(slot))
            .map(|e| std::mem::take(&mut e.metadata))
            .unwrap_or_default();
        copy.metadata = meta.copied(req);
        match archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.at_mut(slot))
        {
            Some(e) => e.metadata = meta,
            None => meta.free(req),
        }
        if plain {
            let read = entry::open_entry_fp(req, id, slot, true).and_then(|opened| {
                if let Err(error) = opened {
                    return Ok(Err(error));
                }
                let (offset, size) = archives(req)
                    .get(id)
                    .and_then(|a| a.manifest.at(slot))
                    .map_or((0, 0), |e| (e.offset, e.uncompressed_filesize as usize));
                entry::efp_read(req, id, &Target::Slot(slot), offset, size).map(Ok)
            });
            let bytes = match read {
                Ok(Ok(bytes)) => bytes,
                failed => {
                    copy.free(req);
                    phar.release_all(req);
                    let error = failed?.err().unwrap_or_default();
                    return Err(throw_exception_ex(
                        req,
                        "UnexpectedValueException",
                        &format!(
                            "Cannot convert phar archive \"{}\", unable to open entry \"{name}\" contents: {error}",
                            lossy(&fname)
                        ),
                    ));
                }
            };
            copy.content = Content::Fp;
            copy.offset = i64::try_from(fp.len()).unwrap_or(i64::MAX);
            fp.extend_from_slice(&bytes);
        }
        if format == Format::Tar {
            copy.tar_type = if is_dir { TAR_DIR } else { TAR_FILE };
        }
        copy.header_offset = 0;
        copy.is_modified = true;
        copy.old_flags = copy.flags & !ENT_COMPRESSION_MASK;
        copy.set_inode(&fname, 3, true);
        let filename = copy.filename.clone();
        if let Err(refused) = phar.manifest.add(&filename, copy) {
            refused.free(req);
        }
        phar.add_virtual_dirs(&filename);
    }
    phar.fp = Some(archive::Fp::written(Arc::new(fp)));
    let new = archives(req).insert(phar);
    let made = rename(req, new, ext);
    match made {
        Ok(Some(object)) => Ok(Some(object)),
        Ok(None) => {
            forget(req, new);
            Ok(None)
        }
        Err(thrown) => {
            forget(req, new);
            Err(thrown)
        }
    }
}

/// The new archive let go where the conversion failed, unless the name
/// table holds it.
fn forget(req: &mut Request, id: ArchiveId) {
    let table = archives(req);
    let held = table.fname_map.iter().any(|(_, held)| *held == id);
    if !held {
        archive::destroy(req, id);
    }
}

/// The extension a conversion puts on where none is asked for.
fn default_ext(a: &Archive) -> &'static [u8] {
    let gz = a.flags & FILE_COMPRESSION_MASK == FILE_COMPRESSED_GZ;
    let bz2 = a.flags & FILE_COMPRESSION_MASK == FILE_COMPRESSED_BZ2;
    match (a.format, a.is_data) {
        (Format::Zip, true) => b"zip",
        (Format::Zip, false) => b"phar.zip",
        (Format::Tar, true) if gz => b"tar.gz",
        (Format::Tar, false) if gz => b"phar.tar.gz",
        (Format::Tar, true) if bz2 => b"tar.bz2",
        (Format::Tar, false) if bz2 => b"phar.tar.bz2",
        (Format::Tar, true) => b"tar",
        (Format::Tar, false) => b"phar.tar",
        (Format::Phar, _) if gz => b"phar.gz",
        (Format::Phar, _) if bz2 => b"phar.bz2",
        (Format::Phar, _) => b"phar",
    }
}

/// `phar_rename_archive`: the new archive named, its extension checked,
/// registered, written with the default stub, and opened as an object.
#[allow(clippy::too_many_lines)] // php's one function, its refusals in its order
fn rename(
    req: &mut Request,
    id: ArchiveId,
    ext: Option<&[u8]>,
) -> Result<Option<Owner<PhpObject>>, Thrown> {
    let Some(a) = archives(req).get(id) else {
        return Ok(None);
    };
    let (is_data, oldpath) = (a.is_data, a.fname.clone());
    let bad =
        |req: &mut Request, m: String| Err(throw_exception_ex(req, "BadMethodCallException", &m));
    let ext: Vec<u8> = match ext {
        None => default_ext(a).to_vec(),
        Some(ext) => {
            if path::check(ext).is_err() {
                let what = if is_data { "data phar" } else { "phar" };
                return bad(
                    req,
                    format!(
                        "{what} converted from \"{}\" has invalid extension {}",
                        lossy(&oldpath),
                        lossy(ext)
                    ),
                );
            }
            ext.to_vec()
        }
    };
    let slash = oldpath
        .iter()
        .rposition(|b| *b == b'/')
        .map_or(0, |s| s + 1);
    let oldname = &oldpath[slash..];
    let mut basename = oldname.to_vec();
    let stripped = PHAR_EXTENSIONS
        .iter()
        .find(|e| oldname.len() > e.len() && oldname.ends_with(e))
        .map(|e| oldname.len() - e.len());
    match stripped {
        Some(at) => basename.truncate(at),
        None => {
            if let Some(dot) = basename.iter().rposition(|b| *b == b'.') {
                basename.truncate(dot);
            }
        }
    }
    let ext = ext.strip_prefix(b".").unwrap_or(&ext).to_vec();
    let newpath = [&oldpath[..slash], &basename, b".", &ext].concat();
    if let Some(a) = archives(req).get_mut(id) {
        a.fname.clone_from(&newpath);
        a.ext_len = ext.len() + 1;
    }
    let shown = lossy(&newpath).into_owned();
    let mut target = id;
    let mut stat_path = newpath.clone();
    if let Some(existing) = archives(req).by_fname(&newpath) {
        let empty = archives(req).get(id).is_some_and(|a| a.manifest.is_empty());
        if !empty {
            return bad(
                req,
                format!(
                    "Unable to add newly converted phar \"{shown}\" to the list of phars, a phar with that name already exists"
                ),
            );
        }
        let Some(mut phar) = archives(req).take(id) else {
            return Ok(None);
        };
        if let Some(pphar) = archives(req).get_mut(existing) {
            pphar.format = phar.format;
            pphar.is_data = phar.is_data;
            pphar.flags = phar.flags;
            pphar.fp = phar.fp.take();
            pphar.refcount += 1;
        }
        phar.alias = None;
        phar.release_all(req);
        target = existing;
        stat_path.clone_from(&oldpath);
    }
    let reused = target != id;
    let unwind = |req: &mut Request| {
        if reused && let Some(a) = archives(req).get_mut(target) {
            a.refcount -= 1;
        }
    };
    if req.url_stat(&stat_path, false, false)?.found().is_some() {
        unwind(req);
        return bad(
            req,
            format!(
                "phar \"{}\" exists and must be unlinked prior to conversion",
                lossy(&stat_path)
            ),
        );
    }
    let Some(a) = archives(req).get(target) else {
        return Ok(None);
    };
    let fname = a.fname.clone();
    let exec = if is_data {
        Exec::Data
    } else {
        Exec::Executable
    };
    let Ok((at, _)) = path::detect_ext(req, &fname, exec, Create::Yes, true)? else {
        unwind(req);
        let what = if is_data { "data phar" } else { "phar" };
        return bad(
            req,
            format!(
                "{what} \"{}\" has invalid extension {}",
                lossy(&fname),
                lossy(&ext)
            ),
        );
    };
    if let Some(a) = archives(req).get_mut(target) {
        a.ext_len = fname.len() - at;
    }
    if !reused && let Some(a) = archives(req).get_mut(target) {
        if is_data || a.is_temporary_alias {
            a.alias = None;
        } else if a.alias.is_some() {
            a.alias = Some(stat_path.clone());
            a.is_temporary_alias = true;
            archives(req).alias_map.update(&stat_path, target);
        }
    }
    if !reused || target == id {
        archives(req).fname_map.update(&stat_path, target);
    }
    if let Some(error) = write::flush_ex(req, target, None, true)? {
        archives(req).fname_map.remove(&stat_path);
        return bad(req, error);
    }
    let class = class_named(req, if is_data { b"PharData" } else { b"Phar" })?;
    let name = string(req, &fname)?;
    let made = instantiate(req, class, &[*name]);
    name.give_up(req);
    Ok(Some(made?))
}
