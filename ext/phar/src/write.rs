//! Writing: `phar_flush_ex` for the phar format, the archive file written
//! whole (a whole-file gzip through ext/zlib's `zlib.deflate`), the
//! default stub `makestub.php` generates (`stub.h`), entries created and
//! removed (`phar_get_or_create_entry_data`, `phar_entry_remove`), and
//! the writing half of the `phar://` wrapper: an entry opened to write,
//! `unlink`, `rename`, `mkdir` and `rmdir`.

use std::sync::Arc;

use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Opening, OwnedStream as _, Request, Streams as _, Whence};

use crate::archive::{
    self, API_VERSION_NODIR, API_VERSION_NUM, ArchiveId, Content, ENT_COMPRESSED_GZ,
    ENT_COMPRESSION_MASK, ENT_PERM_DEF_DIR, ENT_PERM_DEF_FILE, Entry, FILE_COMPRESSED_BZ2,
    FILE_COMPRESSED_GZ, Format, Fp, HDR_SIGNATURE, Metadata, SIG_OPENSSL, SIG_OPENSSL_SHA256,
    SIG_OPENSSL_SHA512, TAR_DIR, TAR_FILE, archives, lossy,
};
use crate::entry::{self, Dir, EntryData, Target};
use crate::open::{Failed, readonly};
use crate::path;
use crate::stream::Report;

/// `time(NULL)` as the archive formats keep it.
pub(crate) fn now() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u32::try_from(d.as_secs()).unwrap_or(u32::MAX))
}

/// `phar_flush`.
pub(crate) fn flush(req: &mut Request, id: ArchiveId) -> Result<Failed, Thrown> {
    flush_ex(req, id, None, false)
}

/// The serialised form of a `metadata` tracker, serialising the value
/// where only it is held (`phar_metadata_tracker_try_ensure_has_serialized_data`).
pub(crate) fn serialized(
    req: &mut Request,
    metadata: &mut Metadata,
) -> Result<Option<Vec<u8>>, Thrown> {
    if metadata.str.is_none()
        && let Some(val) = metadata.val.as_deref().copied()
    {
        let mut out = Vec::new();
        let mut serializer = php_ext_standard::Serializer::new(req);
        let written = serializer.write(req, &mut out, &val);
        serializer.finish(req);
        written?;
        metadata.str = Some(out);
    }
    Ok(metadata.str.clone())
}

/// `php_stristr(stub, "__HALT_COMPILER();")`: where the token begins.
fn find_halt(stub: &[u8]) -> Option<usize> {
    const HALT: &[u8] = b"__HALT_COMPILER();";
    stub.windows(HALT.len())
        .position(|w| w.eq_ignore_ascii_case(HALT))
}

/// `phar_flush_ex` for the phar format: the stub, then the manifest,
/// each entry's bytes (compressed where its flags ask), and the
/// signature, the archive's bytes replaced by them and the file written.
/// Tar and zip archives are their own formats' ([`crate::tar`],
/// [`crate::zip`]). The stub is copied from the file as it stands, as
/// php's stream on it reads it:
///
/// ```php
/// $f = __DIR__ . '/flush-witness.phar';
/// $p = new Phar($f); $p['a'] = '1'; unset($p);
/// $p = new Phar($f);
/// file_put_contents($f, '<?PHP' . substr(file_get_contents($f), 5));
/// $p->setMetadata(1);
/// var_dump(substr(file_get_contents($f), 0, 5), $p['a']->getContent());
/// unset($p); Phar::unlinkArchive($f);
/// ```
/// ```expect regime=phar-writable
/// string(5) "<?PHP"
/// string(1) "1"
/// ```
#[allow(clippy::too_many_lines)] // php's one function, its steps in its order
pub(crate) fn flush_ex(
    req: &mut Request,
    id: ArchiveId,
    user_stub: Option<&[u8]>,
    is_default_stub: bool,
) -> Result<Failed, Thrown> {
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(None);
    };
    if a.manifest.is_empty() && user_stub.is_none() {
        return Ok(None);
    }
    a.virtual_dirs.clear();
    match a.format {
        Format::Zip => return crate::zip::flush(req, id, user_stub, is_default_stub),
        Format::Tar => return crate::tar::flush(req, id, user_stub, is_default_stub),
        Format::Phar => {}
    }
    if readonly(req) {
        return Ok(None);
    }
    let Some(a) = archives(req).get(id) else {
        return Ok(None);
    };
    let fname = a.fname.clone();
    let name = lossy(&fname).into_owned();
    // What the stub and the unchanged entries are copied from.
    let oldfile = archive::read_old(req, id, 0, None)?;
    let mut newfile: Vec<u8> = Vec::new();
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(None);
    };
    if let Some(stub) = user_stub {
        let Some(pos) = find_halt(stub) else {
            return Ok(Some(format!(
                "illegal stub for phar \"{name}\" (__HALT_COMPILER(); is missing)"
            )));
        };
        newfile.extend_from_slice(&stub[..pos + 18]);
        newfile.extend_from_slice(b" ?>\r\n");
        a.halt_offset = newfile.len();
    } else if a.halt_offset != 0 && !a.is_brandnew && oldfile.is_some() {
        let old = oldfile.as_deref().unwrap_or_default();
        let written = a.halt_offset.min(old.len());
        newfile.extend_from_slice(&old[..written]);
        if written != a.halt_offset {
            return Ok(Some(format!(
                "unable to copy stub of old phar to new phar \"{name}\""
            )));
        }
    } else {
        let stub = default_stub(None, None).unwrap_or_default();
        a.halt_offset = stub.len();
        newfile.extend_from_slice(&stub);
    }
    let halt_offset = newfile.len();
    let removable: Vec<usize> = a
        .manifest
        .slots()
        .into_iter()
        .filter(|s| {
            a.manifest
                .at(*s)
                .is_some_and(|e| e.is_deleted && e.can_remove())
        })
        .collect();
    let gone: Vec<Entry> = removable
        .into_iter()
        .filter_map(|slot| a.manifest.remove_at(slot))
        .collect();
    for gone in gone {
        gone.free(req);
    }
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(None);
    };
    let mut main_meta = std::mem::take(&mut a.metadata);
    let main_meta_str = serialized(req, &mut main_meta);
    let Some(a) = archives(req).get_mut(id) else {
        main_meta.free(req);
        return main_meta_str.map(|_| None);
    };
    a.metadata = main_meta;
    let main_meta_str = main_meta_str?;
    let mut global_flags = HDR_SIGNATURE;
    let mut has_dirs = false;
    let mut count = 0_u32;
    let mut offset = 0_usize;
    let mut compressed: Vec<(usize, Vec<u8>)> = Vec::new();
    let slots = a.manifest.slots();
    for &slot in &slots {
        let Some(a) = archives(req).get_mut(id) else {
            return Ok(None);
        };
        let Some(e) = a.manifest.at_mut(slot) else {
            continue;
        };
        if e.is_deleted || e.is_mounted {
            continue;
        }
        count += 1;
        let (fname_bytes, is_dir) = (e.filename.clone(), e.is_dir);
        a.add_virtual_dirs(&fname_bytes);
        has_dirs |= is_dir;
        let Some(e) = a.manifest.at_mut(slot) else {
            continue;
        };
        let mut meta = std::mem::take(&mut e.metadata);
        let meta_str = serialized(req, &mut meta);
        let Some(e) = archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.at_mut(slot))
        else {
            meta.free(req);
            meta_str?;
            continue;
        };
        e.metadata = meta;
        let meta_str = meta_str?;
        offset +=
            4 + e.filename.len() + 24 + meta_str.map_or(0, |m| m.len()) + usize::from(e.is_dir);
        if (oldfile.is_some() && !e.is_modified) || e.is_dir {
            if matches!(e.content, Content::Ufp) {
                e.content = Content::Fp;
            }
            continue;
        }
        let size = e.uncompressed_filesize as usize;
        let flags = e.flags;
        let source = entry::link_source(req, id, slot).unwrap_or(slot);
        let Some(src) = archives(req).get(id).and_then(|a| a.manifest.at(source)) else {
            continue;
        };
        let start = src.offset;
        let data = entry::efp_read(req, id, &Target::Slot(source), start, size)?;
        let crc = !php_ext_standard::crc32::update(u32::MAX, &data);
        let Some(e) = archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.at_mut(slot))
        else {
            continue;
        };
        e.crc32 = crc;
        e.is_crc_checked = true;
        if flags & ENT_COMPRESSION_MASK == 0 {
            e.compressed_filesize = e.uncompressed_filesize;
            continue;
        }
        let entry_name = lossy(&e.filename).into_owned();
        let filter: &[u8] = if flags & ENT_COMPRESSED_GZ != 0 {
            b"zlib.deflate"
        } else {
            b"bzip2.compress"
        };
        let Some(packed) = archive::filter_bytes(req, filter, None, &data)? else {
            let what = if flags & ENT_COMPRESSED_GZ != 0 {
                "gzip"
            } else {
                "bzip2"
            };
            return Ok(Some(format!(
                "unable to {what} compress file \"{entry_name}\" to new phar \"{name}\""
            )));
        };
        let Some(e) = archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.at_mut(slot))
        else {
            continue;
        };
        e.compressed_filesize = u32::try_from(packed.len()).unwrap_or(u32::MAX);
        e.old_flags = e.flags;
        e.is_modified = true;
        global_flags |= e.flags & ENT_COMPRESSION_MASK;
        compressed.push((slot, packed));
    }
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(None);
    };
    let alias: Vec<u8> = if a.is_temporary_alias {
        Vec::new()
    } else {
        a.alias.clone().unwrap_or_default()
    };
    let meta_len = main_meta_str.as_ref().map_or(0, Vec::len);
    let mut manifest_len = u32::try_from(offset + alias.len() + 18 + meta_len).unwrap_or(u32::MAX);
    let padding = matches!(manifest_len.to_le_bytes()[0], b'\r' | b'\n');
    if padding {
        manifest_len += 1;
    }
    let version = if has_dirs {
        API_VERSION_NUM
    } else {
        API_VERSION_NODIR
    };
    newfile.extend_from_slice(&manifest_len.to_le_bytes());
    newfile.extend_from_slice(&count.to_le_bytes());
    newfile.extend_from_slice(&[(version >> 8) as u8, (version & 0xF0) as u8]);
    newfile.extend_from_slice(&global_flags.to_le_bytes());
    newfile.extend_from_slice(&(alias.len() as u32).to_le_bytes());
    newfile.extend_from_slice(&alias);
    newfile.extend_from_slice(&(meta_len as u32).to_le_bytes());
    newfile.extend_from_slice(main_meta_str.as_deref().unwrap_or_default());
    let mytime = now();
    for &slot in &slots {
        let Some(e) = a.manifest.at(slot) else {
            continue;
        };
        if e.is_deleted || e.is_mounted {
            continue;
        }
        let len = e.filename.len() + usize::from(e.is_dir);
        newfile.extend_from_slice(&(len as u32).to_le_bytes());
        newfile.extend_from_slice(&e.filename);
        if e.is_dir {
            newfile.push(b'/');
        }
        let meta = e.metadata.str.as_deref().unwrap_or_default();
        for field in [
            e.uncompressed_filesize,
            mytime,
            e.compressed_filesize,
            e.crc32,
            e.flags,
            meta.len() as u32,
        ] {
            newfile.extend_from_slice(&field.to_le_bytes());
        }
        newfile.extend_from_slice(meta);
    }
    if padding {
        newfile.push((meta_len & 0xFF) as u8);
    }
    // `offset += entry->compressed_filesize`: where each entry is said to
    // stand, whatever a short read left of it, as php counts it.
    let mut running = i64::try_from(newfile.len()).unwrap_or(i64::MAX);
    for &slot in &slots {
        let Some(e) = archives(req).get(id).and_then(|a| a.manifest.at(slot)) else {
            continue;
        };
        if e.is_deleted || e.is_dir || e.is_mounted {
            continue;
        }
        let (csize, start) = (e.compressed_filesize as usize, e.offset);
        let entry_name = lossy(&e.filename).into_owned();
        let bytes = match compressed.iter().position(|(s, _)| *s == slot) {
            Some(at) => compressed.swap_remove(at).1,
            None => match entry::efp(req, id, &Target::Slot(slot))? {
                Some(efp) => entry::read_efp(req, id, &Target::Slot(slot), &efp, start, csize)?,
                None => {
                    return Ok(Some(format!(
                        "unable to seek to start of file \"{entry_name}\" while creating new phar \"{name}\""
                    )));
                }
            },
        };
        let at = running;
        running = running.saturating_add(i64::try_from(csize).unwrap_or(i64::MAX));
        newfile.extend_from_slice(&bytes);
        let Some(e) = archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.at_mut(slot))
        else {
            continue;
        };
        e.offset = at;
        e.offset_abs = at;
        e.is_modified = false;
        if matches!(e.content, Content::Mod(_) | Content::Ufp) {
            e.content = Content::Fp;
        }
    }
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(None);
    };
    a.signature = None;
    let mut sig_flags = a.sig_flags;
    let digest = match crate::signature::create(req, &mut sig_flags, &newfile, &fname)? {
        Ok(digest) => digest,
        Err(error) => {
            return Ok(Some(format!(
                "phar error: unable to write signature: {error}"
            )));
        }
    };
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(None);
    };
    a.sig_flags = sig_flags;
    a.signature = Some(crate::signature::hex(&digest));
    newfile.extend_from_slice(&digest);
    if matches!(
        sig_flags,
        SIG_OPENSSL | SIG_OPENSSL_SHA256 | SIG_OPENSSL_SHA512
    ) {
        newfile.extend_from_slice(&(digest.len() as u32).to_le_bytes());
    }
    newfile.extend_from_slice(&sig_flags.to_le_bytes());
    newfile.extend_from_slice(b"GBMB");
    a.ufp = None;
    a.halt_offset = halt_offset;
    a.is_brandnew = false;
    write_out(req, id, newfile)
}

/// The end of every format's flush: the new bytes the archive's own, and
/// unless the flush is deferred (`startBuffering()`) the file opened
/// `w+b` and written, through `zlib.deflate` where the whole archive is
/// gzip-compressed, which the bytes kept stay uncompressed of.
pub(crate) fn write_out(
    req: &mut Request,
    id: ArchiveId,
    newfile: Vec<u8>,
) -> Result<Failed, Thrown> {
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(None);
    };
    let (fname, flags, deferred) = (a.fname.clone(), a.flags, a.donotflush);
    let newfile = Arc::new(newfile);
    a.set_fp(Fp::Written(Arc::clone(&newfile)));
    if deferred {
        return Ok(None);
    }
    let opening = Opening {
        warn: true,
        ..Opening::default()
    };
    let Some(out) = req.open_stream_with(&fname, b"w+b", opening)? else {
        return Ok(Some(format!(
            "unable to open new phar \"{}\" for writing",
            lossy(&fname)
        )));
    };
    let compressed = |req: &mut Request| -> Result<Option<Vec<u8>>, Thrown> {
        Ok(Some(if flags & FILE_COMPRESSED_GZ != 0 {
            let params = crate::open::window_params(req, 15 + 16)?;
            let packed = archive::filter_bytes(req, b"zlib.deflate", Some(&params), &newfile);
            params.give_up(req);
            let Some(packed) = packed? else {
                return Ok(None);
            };
            packed
        } else if flags & FILE_COMPRESSED_BZ2 != 0 {
            archive::filter_bytes(req, b"bzip2.compress", None, &newfile)?.unwrap_or_default()
        } else {
            newfile.to_vec()
        }))
    };
    let written = match compressed(req) {
        Ok(Some(written)) => written,
        Ok(None) => {
            out.free(req);
            return Ok(Some(format!(
                "unable to compress all contents of phar \"{}\" using zlib, PHP versions older than 5.2.6 have a buggy zlib",
                lossy(&fname)
            )));
        }
        Err(thrown) => {
            out.free(req);
            return Err(thrown);
        }
    };
    let mut at = 0;
    while at < written.len() {
        let end = (at + 8192).min(written.len());
        match out.write(req, &written[at..end]) {
            Some(n) if n > 0 => at += n,
            _ => break,
        }
    }
    out.free(req);
    Ok(None)
}

/// `phar_create_default_stub`: the stub `makestub.php` writes, naming
/// the file the command line includes and the one the web serves,
/// `index.php` for either left out; or php's message for a name longer
/// than 400 bytes.
pub(crate) fn default_stub(index: Option<&[u8]>, web: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let index = index.unwrap_or(b"index.php");
    let web = web.unwrap_or(b"index.php");
    if index.len() > 400 {
        return Err(format!(
            "Illegal filename passed in for stub creation, was {} characters long, and only 400 or less is allowed",
            index.len()
        ));
    }
    if web.len() > 400 {
        return Err(format!(
            "Illegal web filename passed in for stub creation, was {} characters long, and only 400 or less is allowed",
            web.len()
        ));
    }
    Ok(crate::stub::make(index, web))
}

// ---- entries created, removed and copied

/// `phar_get_or_create_entry_data`: an entry opened as
/// [`entry::get_entry_data`] opens it, or created, new and empty, where
/// it is absent and the archive may be written.
pub(crate) fn get_or_create(
    req: &mut Request,
    fname: &[u8],
    path: &[u8],
    mode: &[u8],
    allow_dir: Dir,
    security: bool,
) -> Result<Result<EntryData, Failed>, Thrown> {
    let is_dir = path.last() == Some(&b'/');
    let id = match archive::get_archive(req, fname, None) {
        Ok(id) => id,
        Err(e) => return Ok(Err(e)),
    };
    match entry::get_entry_data(req, fname, path, mode, allow_dir, security)? {
        Err(e) => return Ok(Err(e)),
        Ok(Some(data)) => return Ok(Ok(data)),
        Ok(None) => {}
    }
    let checked = match path::check(path) {
        Ok(checked) => checked,
        Err(e) => {
            return Ok(Err(Some(format!(
                "phar error: invalid path \"{}\" contains {}",
                lossy(path),
                e.words()
            ))));
        }
    };
    let mut name = checked.as_slice();
    if is_dir && !name.is_empty() {
        name = &name[..name.len() - 1];
    }
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(Err(None));
    };
    let mut e = Entry::new(name);
    e.content = Content::Mod(Vec::new());
    e.fp_refcount = 1;
    e.flags = if allow_dir == Dir::Only {
        e.is_dir = true;
        ENT_PERM_DEF_DIR
    } else {
        ENT_PERM_DEF_FILE
    };
    e.old_flags = e.flags;
    a.add_virtual_dirs(name);
    e.is_modified = true;
    e.timestamp = now();
    e.is_crc_checked = true;
    if a.is_tar() {
        e.tar_type = if e.is_dir { TAR_DIR } else { TAR_FILE };
    }
    let real = a.fname.clone();
    match a.manifest.add(name, e) {
        Ok(slot) => {
            a.refcount += 1;
            Ok(Ok(EntryData {
                archive: id,
                target: Target::Slot(slot),
                fp: Some(entry::Efp::Own),
                position: 0,
                zero: 0,
            }))
        }
        Err(_) => Ok(Err(Some(format!(
            "phar error: unable to add new entry \"{}\" to phar \"{}\"",
            lossy(name),
            lossy(&real)
        )))),
    }
}

/// `phar_entry_remove`: an unlinked entry deleted from the manifest where
/// nothing else holds it, else marked deleted, and the archive written
/// unless writes are deferred.
pub(crate) fn remove(req: &mut Request, data: EntryData) -> Result<Failed, Thrown> {
    let id = data.archive;
    let Target::Slot(slot) = data.target else {
        entry::delref(req, data);
        return Ok(None);
    };
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(None);
    };
    let removable = a
        .manifest
        .at(slot)
        .is_some_and(|e| e.fp_refcount < 2 && e.fileinfo_lock_count == 0);
    if removable {
        if let Some(gone) = a.manifest.remove_at(slot) {
            a.refcount -= 1;
            gone.free(req);
        }
    } else {
        if let Some(e) = a.manifest.at_mut(slot) {
            e.is_deleted = true;
        }
        entry::delref(req, data);
    }
    if archives(req).get(id).is_some_and(|a| !a.donotflush) {
        return flush(req, id);
    }
    Ok(None)
}

/// `phar_copy_entry_fp`: `source`'s contents, from the end of its links,
/// to be another entry's own.
pub(crate) fn copy_contents(
    req: &mut Request,
    id: ArchiveId,
    source: usize,
) -> Result<Result<Vec<u8>, String>, Thrown> {
    if let Err(error) = entry::open_entry_fp(req, id, source, true)? {
        return Ok(Err(error));
    }
    let from = entry::link_source(req, id, source).unwrap_or(source);
    Ok(Ok(entry::contents(req, id, from)?))
}

// ---- the stream's writing half

/// `phar_stream_write`: `buf` into `idata->fp` at the stream's position,
/// the entry's sizes grown to it and the entry marked modified. That is
/// the entry's own buffer once written, or the archive's bytes the entry
/// was opened to read where php holds them through a stream it wrote
/// (the file the flush opened `w+b`, a temporary one); through the file
/// opened `rb` the plain wrapper's write fails, with its notice.
///
/// ```php
/// $f = __DIR__ . '/write-witness.phar';
/// $p = new Phar($f); $p['a'] = '1'; unset($p);
/// $p = new Phar($f);
/// $e = $p['a']->openFile();
/// var_dump($e->fwrite('2'));
/// $p['b'] = '3';
/// $e = $p['b']->openFile();
/// $e->fseek(1);
/// var_dump($e->fwrite('4'));
/// $e->rewind();
/// var_dump($e->fgets());
/// unset($e, $p); Phar::unlinkArchive($f);
/// ```
/// ```expect regime=phar-writable
/// Notice: SplFileObject::fwrite(): Write of 1 bytes failed with errno=9 Bad file descriptor in %s on line 6
/// bool(false)
/// int(1)
/// string(2) "34"
/// ```
pub(crate) fn stream_write(req: &mut Request, data: &mut EntryData, buf: &[u8]) -> Option<usize> {
    let Target::Slot(slot) = data.target else {
        return None;
    };
    let at = usize::try_from(data.position + data.zero).ok()?;
    let archive::Archive {
        manifest, fp, ufp, ..
    } = archives(req).get_mut(data.archive)?;
    let e = manifest.at_mut(slot)?;
    let end = match (&mut e.content, fp, ufp) {
        (Content::Mod(own), _, _) => splice(own, at, buf),
        (Content::Fp, Some(Fp::Written(bytes)), _) | (Content::Ufp, _, Some(bytes)) => {
            let end = splice(Arc::make_mut(bytes), at, buf);
            data.fp = Some(entry::Efp::Shared(Arc::clone(bytes)));
            end
        }
        (Content::Fp, Some(Fp::File(stream)), _) => {
            let stream = **stream;
            stream.seek(req, i64::try_from(at).ok()?, Whence::Set);
            stream.write(req, buf);
            return None;
        }
        _ => return None,
    };
    data.position = i64::try_from(end).ok()? - data.zero;
    if data.position > i64::from(e.uncompressed_filesize) {
        e.uncompressed_filesize = u32::try_from(data.position).unwrap_or(u32::MAX);
    }
    e.compressed_filesize = e.uncompressed_filesize;
    e.old_flags = e.flags;
    e.is_modified = true;
    Some(buf.len())
}

/// `buf` written over `bytes` at `at`, the bytes grown with NULs to reach
/// it; where the write ends.
fn splice(bytes: &mut Vec<u8>, at: usize, buf: &[u8]) -> usize {
    let end = at + buf.len();
    if bytes.len() < end {
        bytes.resize(end, 0);
    }
    bytes[at..end].copy_from_slice(buf);
    end
}

/// `phar_stream_flush`: an entry something wrote to stamped and its
/// archive written, a failure warned of; `false` for php's `EOF`.
pub(crate) fn stream_flush(req: &mut Request, data: &EntryData) -> Result<bool, Thrown> {
    let Target::Slot(slot) = data.target else {
        return Ok(false);
    };
    let Some(e) = archives(req)
        .get_mut(data.archive)
        .and_then(|a| a.manifest.at_mut(slot))
    else {
        return Ok(false);
    };
    if !e.is_modified {
        return Ok(false);
    }
    e.timestamp = now();
    let failed = flush(req, data.archive)?;
    if let Some(error) = failed {
        docref(req, Level::Warning, &error);
        return Ok(false);
    }
    Ok(true)
}

/// `phar_wrapper_open_url` for a write: the entry opened or created, its
/// compression and `metadata` taken from the `phar` options of the
/// stream's `stream_context`.
pub(crate) fn open_for_write(
    req: &mut Request,
    host: &[u8],
    internal: &[u8],
    mode: &[u8],
) -> Result<Option<EntryData>, Thrown> {
    let data = match get_or_create(req, host, internal, mode, Dir::No, true)? {
        Ok(data) => data,
        Err(Some(error)) => {
            req.wrapper_log_error(&error);
            return Ok(None);
        }
        Err(None) => {
            req.wrapper_log_error(&format!(
                "phar error: file \"{}\" could not be created in phar \"{}\"",
                lossy(internal),
                lossy(host)
            ));
            return Ok(None);
        }
    };
    if let Some(context) = req.stream_ctx() {
        let compress = context.option(req, b"phar", b"compress");
        let metadata = context.option(req, b"phar", b"metadata");
        if let Target::Slot(slot) = data.target {
            if let Some(level) = compress.and_then(|v| v.as_int())
                && level & !i64::from(ENT_COMPRESSION_MASK) == 0
                && let Some(e) = archives(req)
                    .get_mut(data.archive)
                    .and_then(|a| a.manifest.at_mut(slot))
                && e.uncompressed_filesize == 0
                && e.compressed_filesize == 0
            {
                e.flags &= !ENT_COMPRESSION_MASK;
                e.flags |= u32::try_from(level).unwrap_or(0);
            }
            if let Some(value) = metadata {
                let value = php_ext::values::Owner::take(req, value.deref());
                if let Some(a) = archives(req).get_mut(data.archive) {
                    a.is_modified = true;
                }
                let mut kept = Metadata {
                    val: Some(value),
                    str: None,
                };
                if let Some(e) = archives(req)
                    .get_mut(data.archive)
                    .and_then(|a| a.manifest.at_mut(slot))
                {
                    std::mem::swap(&mut e.metadata, &mut kept);
                }
                kept.free(req);
            }
        }
    }
    Ok(Some(data))
}

// ---- the wrapper's other writing operations

/// Whether the archive `host` names is a data archive, as the checks
/// before a write read it.
fn is_data(req: &mut Request, host: &[u8]) -> bool {
    let table = archives(req);
    table
        .by_fname(host)
        .and_then(|id| table.get(id))
        .is_some_and(|a| a.is_data)
}

/// `phar_wrapper_unlink`: an entry deleted and the archive written, each
/// refusal warned of, as php hands `unlink` `REPORT_ERRORS`.
pub(crate) fn unlink(req: &mut Request, url: &[u8]) -> Result<bool, Thrown> {
    let Some(parsed) = crate::stream::parse_url(req, url, b"rb", Report::Warn)? else {
        docref(req, Level::Warning, "phar error: unlink failed");
        return Ok(false);
    };
    if readonly(req) && !is_data(req, &parsed.host) {
        docref(
            req,
            Level::Warning,
            "phar error: write operations disabled by the php.ini setting phar.readonly",
        );
        return Ok(false);
    }
    let internal = parsed.path[1..].to_vec();
    let data = match entry::get_entry_data(req, &parsed.host, &internal, b"r", Dir::No, true)? {
        Ok(Some(data)) => data,
        Ok(None) | Err(None) => {
            docref(
                req,
                Level::Warning,
                &format!("unlink of \"{}\" failed, file does not exist", lossy(url)),
            );
            return Ok(false);
        }
        Err(Some(error)) => {
            docref(
                req,
                Level::Warning,
                &format!("unlink of \"{}\" failed: {error}", lossy(url)),
            );
            return Ok(false);
        }
    };
    let refs = match data.target {
        Target::Slot(slot) => archives(req)
            .get(data.archive)
            .and_then(|a| a.manifest.at(slot))
            .map_or(0, |e| e.fp_refcount),
        Target::Temp(_) => 0,
    };
    if refs > 1 {
        docref(
            req,
            Level::Warning,
            &format!(
                "phar error: \"{}\" in phar \"{}\", has open file pointers, cannot unlink",
                lossy(&internal),
                lossy(&parsed.host)
            ),
        );
        entry::delref(req, data);
        return Ok(false);
    }
    if let Some(error) = remove(req, data)? {
        docref(req, Level::Warning, &error);
    }
    Ok(true)
}

/// `phar_wrapper_rename`: an entry, or a directory and everything under
/// it, renamed inside one archive and the archive written.
#[allow(clippy::too_many_lines)] // php's one function, its refusals in its order
pub(crate) fn rename(req: &mut Request, from: &[u8], to: &[u8]) -> Result<bool, Thrown> {
    let (shown_from, shown_to) = (lossy(from).into_owned(), lossy(to).into_owned());
    let fail = |req: &mut Request, tail: String| {
        docref(req, Level::Warning, &format!("phar error: {tail}"));
        Ok(false)
    };
    let Some(src) = crate::stream::parse_url(req, from, b"wb", Report::Log)? else {
        return fail(
            req,
            format!(
                "cannot rename \"{shown_from}\" to \"{shown_to}\": invalid or non-writable url \"{shown_from}\""
            ),
        );
    };
    let from_data = archive::get_archive(req, &src.host, None)
        .ok()
        .and_then(|id| archives(req).get(id))
        .is_some_and(|a| a.is_data);
    if readonly(req) && !from_data {
        return fail(
            req,
            "Write operations disabled by the php.ini setting phar.readonly".into(),
        );
    }
    let Some(dst) = crate::stream::parse_url(req, to, b"wb", Report::Log)? else {
        return fail(
            req,
            format!(
                "cannot rename \"{shown_from}\" to \"{shown_to}\": invalid or non-writable url \"{shown_to}\""
            ),
        );
    };
    let to_data = archive::get_archive(req, &dst.host, None)
        .ok()
        .and_then(|id| archives(req).get(id))
        .is_some_and(|a| a.is_data);
    if readonly(req) && !to_data {
        return fail(
            req,
            "Write operations disabled by the php.ini setting phar.readonly".into(),
        );
    }
    if src.host != dst.host {
        return fail(
            req,
            format!(
                "cannot rename \"{shown_from}\" to \"{shown_to}\", not within the same phar archive"
            ),
        );
    }
    let id = match archive::get_archive(req, &src.host, None) {
        Ok(id) => id,
        Err(error) => {
            let error = error.unwrap_or_default();
            return fail(
                req,
                format!("cannot rename \"{shown_from}\" to \"{shown_to}\": {error}"),
            );
        }
    };
    let (old, new) = (src.path[1..].to_vec(), dst.path[1..].to_vec());
    let mut is_modified = false;
    let slot = archives(req).get(id).and_then(|a| a.manifest.slot(&old));
    let is_dir = if let Some(slot) = slot {
        let Some(a) = archives(req).get_mut(id) else {
            return Ok(false);
        };
        let Some(e) = a.manifest.at_mut(slot) else {
            return Ok(false);
        };
        if e.is_deleted {
            return fail(
                req,
                format!(
                    "cannot rename \"{shown_from}\" to \"{shown_to}\" from extracted phar archive, source has been deleted"
                ),
            );
        }
        let mut moved = Entry::new(&new);
        moved.uncompressed_filesize = e.uncompressed_filesize;
        moved.timestamp = e.timestamp;
        moved.compressed_filesize = e.compressed_filesize;
        moved.crc32 = e.crc32;
        moved.flags = e.flags;
        moved.old_flags = e.old_flags;
        moved.metadata = Metadata {
            val: None,
            str: e.metadata.str.clone(),
        };
        moved.metadata.val = e.metadata.val.take();
        moved.offset = e.offset;
        moved.offset_abs = e.offset_abs;
        moved.header_offset = e.header_offset;
        moved.tar_type = e.tar_type;
        moved.inode = e.inode;
        moved.is_crc_checked = e.is_crc_checked;
        moved.is_dir = e.is_dir;
        moved.link = e.link.take();
        e.is_deleted = true;
        let dir = moved.is_dir;
        let (_, replaced) = a.manifest.update(&new, moved);
        if let Some(replaced) = replaced {
            replaced.free(req);
        }
        let Some(new_slot) = archives(req).get(id).and_then(|a| a.manifest.slot(&new)) else {
            return Ok(false);
        };
        match copy_contents(req, id, slot)? {
            Ok(bytes) => {
                if let Some(e) = archives(req)
                    .get_mut(id)
                    .and_then(|a| a.manifest.at_mut(new_slot))
                {
                    e.link = None;
                    e.content = Content::Mod(bytes);
                    e.offset = 0;
                    e.is_modified = true;
                }
            }
            Err(error) => {
                if let Some(gone) = archives(req)
                    .get_mut(id)
                    .and_then(|a| a.manifest.remove(&new))
                {
                    gone.free(req);
                }
                return fail(
                    req,
                    format!("cannot rename \"{shown_from}\" to \"{shown_to}\": {error}"),
                );
            }
        }
        is_modified = true;
        dir
    } else {
        let dir = archives(req)
            .get(id)
            .is_some_and(|a| a.virtual_dirs.contains(&old));
        if !dir {
            return fail(
                req,
                format!(
                    "cannot rename \"{shown_from}\" to \"{shown_to}\" from extracted phar archive, source does not exist"
                ),
            );
        }
        true
    };
    if is_dir {
        let Some(a) = archives(req).get_mut(id) else {
            return Ok(false);
        };
        let under = |key: &[u8], exact: bool| {
            key.len() >= old.len()
                && key.starts_with(&old)
                && ((exact && key.len() == old.len()) || key.get(old.len()) == Some(&b'/'))
        };
        let moved = |key: &[u8]| [new.as_slice(), &key[old.len()..]].concat();
        for slot in a.manifest.slots() {
            let key = a
                .manifest
                .key_at(slot)
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            let live = a.manifest.at(slot).is_some_and(|e| !e.is_deleted);
            if live && key.len() > old.len() && under(&key, false) {
                let renamed = moved(&key);
                if let Some(e) = a.manifest.at_mut(slot) {
                    e.is_modified = true;
                    e.filename.clone_from(&renamed);
                }
                is_modified = true;
                a.manifest.rekey(slot, &renamed);
            }
        }
        for table in [&mut a.virtual_dirs, &mut a.mounted_dirs] {
            for slot in table.slots() {
                let key = table.key_at(slot).map(<[u8]>::to_vec).unwrap_or_default();
                if under(&key, true) {
                    table.rekey(slot, &moved(&key));
                }
            }
        }
    }
    if is_modified && let Some(error) = flush(req, id)? {
        return fail(
            req,
            format!("cannot rename \"{shown_from}\" to \"{shown_to}\": {error}"),
        );
    }
    Ok(true)
}

/// `phar_wrapper_mkdir`: a directory entry added and the archive written.
#[allow(clippy::too_many_lines)] // php's one function, its refusals in its order
pub(crate) fn mkdir(
    req: &mut Request,
    url: &[u8],
    _mode: i64,
    _recursive: bool,
) -> Result<bool, Thrown> {
    let shown = lossy(url).into_owned();
    let split = path::split_fname(req, url, path::Exec::Either, path::Create::Either)?;
    let Ok(split) = split else {
        docref(
            req,
            Level::Warning,
            &format!("phar error: cannot create directory \"{shown}\", no phar archive specified"),
        );
        return Ok(false);
    };
    let data_archive = archive::get_archive(req, &split.arch, None)
        .ok()
        .and_then(|id| archives(req).get(id))
        .is_some_and(|a| a.is_data);
    if readonly(req) && !data_archive {
        docref(
            req,
            Level::Warning,
            &format!("phar error: cannot create directory \"{shown}\", write operations disabled"),
        );
        return Ok(false);
    }
    let Some(parsed) = crate::stream::parse_url(req, url, b"w", Report::Warn)? else {
        return Ok(false);
    };
    let (dir, host) = (parsed.path[1..].to_vec(), parsed.host);
    let (d, h) = (lossy(&dir).into_owned(), lossy(&host).into_owned());
    let id = match archive::get_archive(req, &host, None) {
        Ok(id) => id,
        Err(error) => {
            let error = error.unwrap_or_default();
            docref(
                req,
                Level::Warning,
                &format!(
                    "phar error: cannot create directory \"{d}\" in phar \"{h}\", error retrieving phar information: {error}"
                ),
            );
            return Ok(false);
        }
    };
    match entry::find(req, id, &dir, Dir::Only, true)? {
        Ok(_) => {
            docref(
                req,
                Level::Warning,
                &format!(
                    "phar error: cannot create directory \"{d}\" in phar \"{h}\", directory already exists"
                ),
            );
            return Ok(false);
        }
        Err(Some(error)) => {
            docref(
                req,
                Level::Warning,
                &format!("phar error: cannot create directory \"{d}\" in phar \"{h}\", {error}"),
            );
            return Ok(false);
        }
        Err(None) => {}
    }
    match entry::find(req, id, &dir, Dir::No, true)? {
        Ok(_) => {
            docref(
                req,
                Level::Warning,
                &format!(
                    "phar error: cannot create directory \"{d}\" in phar \"{h}\", file already exists"
                ),
            );
            return Ok(false);
        }
        Err(Some(error)) => {
            docref(
                req,
                Level::Warning,
                &format!("phar error: cannot create directory \"{d}\" in phar \"{h}\", {error}"),
            );
            return Ok(false);
        }
        Err(None) => {}
    }
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(false);
    };
    let mut e = Entry::new(&dir);
    if a.is_tar() {
        e.tar_type = TAR_DIR;
    }
    e.is_dir = true;
    e.is_modified = true;
    e.is_crc_checked = true;
    e.flags = ENT_PERM_DEF_DIR;
    e.old_flags = ENT_PERM_DEF_DIR;
    let real = a.fname.clone();
    if a.manifest.add(&dir, e).is_err() {
        docref(
            req,
            Level::Warning,
            &format!(
                "phar error: cannot create directory \"{d}\" in phar \"{}\", adding to manifest failed",
                lossy(&real)
            ),
        );
        return Ok(false);
    }
    if let Some(error) = flush(req, id)? {
        docref(
            req,
            Level::Warning,
            &format!(
                "phar error: cannot create directory \"{d}\" in phar \"{}\", {error}",
                lossy(&real)
            ),
        );
        if let Some(gone) = archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.remove(&dir))
        {
            gone.free(req);
        }
        return Ok(false);
    }
    if let Some(a) = archives(req).get_mut(id) {
        a.add_virtual_dirs(&dir);
    }
    Ok(true)
}

/// `phar_wrapper_rmdir`: an empty directory removed and the archive
/// written.
#[allow(clippy::too_many_lines)] // php's one function, its refusals in its order
pub(crate) fn rmdir(req: &mut Request, url: &[u8]) -> Result<bool, Thrown> {
    let shown = lossy(url).into_owned();
    let split = path::split_fname(req, url, path::Exec::Either, path::Create::Either)?;
    let Ok(split) = split else {
        docref(
            req,
            Level::Warning,
            &format!(
                "phar error: cannot remove directory \"{shown}\", no phar archive specified, or phar archive does not exist"
            ),
        );
        return Ok(false);
    };
    let data_archive = archive::get_archive(req, &split.arch, None)
        .ok()
        .and_then(|id| archives(req).get(id))
        .is_some_and(|a| a.is_data);
    if readonly(req) && !data_archive {
        docref(
            req,
            Level::Warning,
            &format!("phar error: cannot rmdir directory \"{shown}\", write operations disabled"),
        );
        return Ok(false);
    }
    let Some(parsed) = crate::stream::parse_url(req, url, b"w", Report::Warn)? else {
        return Ok(false);
    };
    let (dir, host) = (parsed.path[1..].to_vec(), parsed.host);
    let (d, h) = (lossy(&dir).into_owned(), lossy(&host).into_owned());
    let id = match archive::get_archive(req, &host, None) {
        Ok(id) => id,
        Err(error) => {
            let error = error.unwrap_or_default();
            docref(
                req,
                Level::Warning,
                &format!(
                    "phar error: cannot remove directory \"{d}\" in phar \"{h}\", error retrieving phar information: {error}"
                ),
            );
            return Ok(false);
        }
    };
    let found = match entry::find(req, id, &dir, Dir::Only, true)? {
        Ok(found) => found,
        Err(Some(error)) => {
            docref(
                req,
                Level::Warning,
                &format!("phar error: cannot remove directory \"{d}\" in phar \"{h}\", {error}"),
            );
            return Ok(false);
        }
        Err(None) => {
            docref(
                req,
                Level::Warning,
                &format!(
                    "phar error: cannot remove directory \"{d}\" in phar \"{h}\", directory does not exist"
                ),
            );
            return Ok(false);
        }
    };
    let Some(a) = archives(req).get(id) else {
        return Ok(false);
    };
    let deleted = match &found {
        entry::Found::Slot(slot) => a.manifest.at(*slot).is_some_and(|e| e.is_deleted),
        entry::Found::TempDir(_) => false,
    };
    let beneath = |key: &[u8]| key.starts_with(&dir) && key.get(dir.len()) == Some(&b'/');
    if !deleted
        && (a.manifest.iter().any(|(k, _)| beneath(k))
            || a.virtual_dirs.iter().any(|(k, ())| beneath(k)))
    {
        docref(req, Level::Warning, "phar error: Directory not empty");
        return Ok(false);
    }
    match found {
        entry::Found::TempDir(_) => {
            if let Some(a) = archives(req).get_mut(id) {
                a.virtual_dirs.remove(&dir);
            }
        }
        entry::Found::Slot(slot) => {
            let Some(e) = archives(req)
                .get_mut(id)
                .and_then(|a| a.manifest.at_mut(slot))
            else {
                return Ok(false);
            };
            e.is_deleted = true;
            e.is_modified = true;
            let name = lossy(&e.filename).into_owned();
            let real = archives(req)
                .get(id)
                .map(|a| lossy(&a.fname).into_owned())
                .unwrap_or_default();
            if let Some(error) = flush(req, id)? {
                docref(
                    req,
                    Level::Warning,
                    &format!(
                        "phar error: cannot remove directory \"{name}\" in phar \"{real}\", {error}"
                    ),
                );
                return Ok(false);
            }
        }
    }
    Ok(true)
}
