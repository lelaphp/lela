//! What `Phar`'s methods do to an archive's entries and its stub:
//! `phar_add_file`, `phar_mkdir`, `phar_build` over an iterator, `copy`,
//! the stub read and set, the alias set, and `unlinkArchive`.

use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use php_ext::abi::{Passed, Raw};
use php_ext::call::{instantiate, lookup_class};
use php_ext::coerce::{value_name, with_string};
use php_ext::raising::{Thrown, arity, deprecated, throw_exception_ex};
use php_ext::request::{OwnedStream as _, Request, Stream, Streams as _};
use php_ext::values::{ClassId, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};
use php_ext_spl::directory::{self as spl, Filesystem};

use crate::archive::{
    self, ArchiveId, Content, ENT_COMPRESSED_BZ2, ENT_COMPRESSED_GZ, ENT_COMPRESSION_MASK,
    ENT_PERM_MASK, archives, lossy, string,
};
use crate::entry::{self, Dir, Target};
use crate::object::{class_named, construct_as};
use crate::open::{self, readonly};
use crate::path;
use crate::throwables::Throwables;
use crate::write;

/// What an entry is filled from: a string's bytes or a stream read to its
/// end.
#[derive(Clone, Copy)]
pub(crate) enum Source<'a> {
    Bytes(&'a [u8]),
    Stream(Stream),
}

/// A flush's error as `PharException`.
fn flushed(req: &mut Request, id: ArchiveId) -> Result<(), Thrown> {
    match write::flush(req, id)? {
        Some(error) => {
            let ce = Throwables::of(req)?;
            Err(throw_exception_ex(req, ce.phar_exception, &error))
        }
        None => Ok(()),
    }
}

/// The archive's name, as the messages print it.
fn fname_of(req: &mut Request, id: ArchiveId) -> Vec<u8> {
    archives(req)
        .get(id)
        .map(|a| a.fname.clone())
        .unwrap_or_default()
}

/// `phar_add_file`: the entry opened or created, filled from `source`,
/// its permissions a stream's where it has them and else the default
/// less the umask, and the archive written.
pub(crate) fn add_file(
    req: &mut Request,
    id: ArchiveId,
    name: &[u8],
    source: Source<'_>,
) -> Result<(), Thrown> {
    if path::is_magic(name) {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.bad_method_call_exception,
            "Cannot create any files in magic \".phar\" directory",
        ));
    }
    let fname = fname_of(req, id);
    let mut data = match write::get_or_create(req, &fname, name, b"w+b", Dir::No, true)? {
        Ok(data) => data,
        Err(error) => {
            let tail = error.map(|e| format!(": {e}")).unwrap_or_default();
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.bad_method_call_exception,
                &format!(
                    "Entry {} does not exist and cannot be created{tail}",
                    lossy(name)
                ),
            ));
        }
    };
    let slot = match &data.target {
        Target::Slot(slot) => Some(*slot),
        Target::Temp(_) => None,
    };
    let is_dir = slot
        .and_then(|slot| archives(req).get(id).and_then(|a| a.manifest.at(slot)))
        .is_some_and(|e| e.is_dir);
    let mut stream = None;
    if !is_dir {
        let bytes = match source {
            Source::Bytes(bytes) => bytes.to_vec(),
            Source::Stream(from) => {
                stream = Some(from);
                archive::copy_all(req, from)?
            }
        };
        if !bytes.is_empty() && write::stream_write(req, &mut data, &bytes) != Some(bytes.len()) {
            entry::delref(req, data);
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.bad_method_call_exception,
                &format!("Entry {} could not be written to", lossy(name)),
            ));
        }
        if let Some(e) = slot.and_then(|slot| {
            archives(req)
                .get_mut(id)
                .and_then(|a| a.manifest.at_mut(slot))
        }) {
            let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
            e.compressed_filesize = len;
            e.uncompressed_filesize = len;
        }
    }
    let mode = stream.and_then(|s| s.stat(req)).map(|sb| sb[2]);
    let umask = req.environment().umask();
    if let Some(e) = slot.and_then(|slot| {
        archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.at_mut(slot))
    }) {
        match mode {
            Some(mode) => e.flags = u32::try_from(mode).unwrap_or(0) & ENT_PERM_MASK,
            None => e.flags &= !umask,
        }
    }
    entry::delref(req, data);
    flushed(req, id)
}

/// `phar_mkdir`: the directory entry opened or created, and the archive
/// written.
pub(crate) fn mkdir(req: &mut Request, id: ArchiveId, dir: &[u8]) -> Result<(), Thrown> {
    let fname = fname_of(req, id);
    let data = match write::get_or_create(req, &fname, dir, b"w+b", Dir::Only, true)? {
        Ok(data) => data,
        Err(error) => {
            let tail = error.map(|e| format!(": {e}")).unwrap_or_default();
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.bad_method_call_exception,
                &format!(
                    "Directory {} does not exist and cannot be created{tail}",
                    lossy(dir)
                ),
            ));
        }
    };
    entry::delref(req, data);
    flushed(req, id)
}

/// `Phar::copy()`: the entry's row copied under a new name, its contents
/// the copy's own where they are not the archive's file, and the archive
/// written.
pub(crate) fn copy(req: &mut Request, id: ArchiveId, old: &[u8], new: &[u8]) -> Result<(), Thrown> {
    let fname = lossy(&fname_of(req, id)).into_owned();
    let (o, n) = (lossy(old).into_owned(), lossy(new).into_owned());
    let is_data = archives(req).get(id).is_some_and(|a| a.is_data);
    if readonly(req) && !is_data {
        return refuse(
            req,
            &format!("Cannot copy \"{o}\" to \"{n}\", phar is read-only"),
        );
    }
    if path::is_magic(old) {
        return refuse(
            req,
            &format!(
                "file \"{o}\" cannot be copied to file \"{n}\", cannot copy Phar meta-file in {fname}"
            ),
        );
    }
    if path::is_magic(new) {
        return refuse(
            req,
            &format!(
                "file \"{o}\" cannot be copied to file \"{n}\", cannot copy to Phar meta-file in {fname}"
            ),
        );
    }
    let Some(a) = archives(req).get(id) else {
        return Ok(());
    };
    let source = match a.manifest.slot(old) {
        Some(slot) if a.manifest.at(slot).is_some_and(|e| !e.is_deleted) => slot,
        _ => {
            return refuse(
                req,
                &format!(
                    "file \"{o}\" cannot be copied to file \"{n}\", file does not exist in {fname}"
                ),
            );
        }
    };
    if a.manifest.get(new).is_some_and(|e| !e.is_deleted) {
        return refuse(
            req,
            &format!(
                "file \"{o}\" cannot be copied to file \"{n}\", file must not already exist in phar {fname}"
            ),
        );
    }
    if let Err(e) = path::check(new) {
        return refuse(
            req,
            &format!(
                "file \"{n}\" contains invalid characters {}, cannot be copied from \"{o}\" in phar {fname}",
                e.words()
            ),
        );
    }
    let own = match a
        .manifest
        .at(source)
        .map(|e| matches!(e.content, Content::Fp))
    {
        Some(false) => match write::copy_contents(req, id, source)? {
            Ok(bytes) => Some(bytes),
            Err(error) => return copy_failed(req, &error),
        },
        _ => None,
    };
    let Some(from) = archives(req)
        .get_mut(id)
        .and_then(|a| a.manifest.at_mut(source))
    else {
        return Ok(());
    };
    let mut kept = std::mem::take(&mut from.metadata);
    let mut copied = kept.copied(req);
    let Some(a) = archives(req).get_mut(id) else {
        kept.free(req);
        copied.free(req);
        return Ok(());
    };
    let Some(from) = a.manifest.at_mut(source) else {
        kept.free(req);
        copied.free(req);
        return Ok(());
    };
    from.metadata = kept;
    let mut copy = from.shallow(new);
    copy.metadata = copied;
    if let Some(bytes) = own {
        copy.content = Content::Mod(bytes);
        copy.offset = 0;
        copy.is_modified = true;
    }
    let refused = a.manifest.add(new, copy).err();
    a.is_modified = true;
    if let Some(refused) = refused {
        refused.free(req);
    }
    flushed(req, id)
}

/// `copy`'s refusal, an `UnexpectedValueException`.
fn refuse<T>(req: &mut Request, m: &str) -> Result<T, Thrown> {
    let ce = Throwables::of(req)?;
    Err(throw_exception_ex(req, ce.unexpected_value_exception, m))
}

/// `copy`'s failure to copy an entry's contents, a `PharException`.
fn copy_failed<T>(req: &mut Request, error: &str) -> Result<T, Thrown> {
    let ce = Throwables::of(req)?;
    Err(throw_exception_ex(req, ce.phar_exception, error))
}

/// `Phar::getStub()`: the phar format's bytes before the manifest, or a
/// tar or zip archive's `.phar/stub.php`.
pub(crate) fn stub_of(req: &mut Request, id: ArchiveId) -> Result<Vec<u8>, Thrown> {
    let Some(a) = archives(req).get(id) else {
        return Ok(Vec::new());
    };
    let fname = a.fname.clone();
    let shared =
        a.fp.as_ref()
            .map(|fp| Arc::clone(&fp.bytes))
            .filter(|_| !a.is_brandnew);
    if a.is_tar() || a.is_zip() {
        let Some(stub) = a.manifest.get(b".phar/stub.php") else {
            return Ok(Vec::new());
        };
        let (at, len, csize, flags) = (
            stub.offset_abs,
            stub.uncompressed_filesize as usize,
            stub.compressed_filesize as usize,
            stub.flags,
        );
        let compressed = flags & ENT_COMPRESSION_MASK != 0;
        let (bytes, csize) = if let Some(bytes) = shared.filter(|_| !compressed) {
            (bytes, len)
        } else {
            let Some(bytes) = archive::read_file(req, &fname, false)? else {
                let ce = Throwables::of(req)?;
                return Err(throw_exception_ex(
                    req,
                    ce.unexpected_value_exception,
                    &format!("phar error: unable to open phar \"{}\"", lossy(&fname)),
                ));
            };
            (Arc::new(bytes), csize)
        };
        let start = usize::try_from(at).unwrap_or(usize::MAX).min(bytes.len());
        let raw = &bytes[start..(start + csize).min(bytes.len())];
        let out = if compressed {
            let filter: &[u8] = if flags & ENT_COMPRESSED_GZ != 0 {
                b"zlib.inflate"
            } else {
                b"bzip2.decompress"
            };
            let Some(out) = archive::filter_bytes(req, filter, None, raw)? else {
                let kind = if flags & ENT_COMPRESSED_BZ2 != 0 {
                    "bzip2"
                } else {
                    "zlib"
                };
                let ce = Throwables::of(req)?;
                return Err(throw_exception_ex(
                    req,
                    ce.unexpected_value_exception,
                    &format!(
                        "phar error: unable to read stub of phar \"{}\" (cannot create {kind} filter)",
                        lossy(&fname)
                    ),
                ));
            };
            out
        } else {
            raw.to_vec()
        };
        if out.len() < len {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.runtime_exception,
                "Unable to read stub",
            ));
        }
        return Ok(out[..len].to_vec());
    }
    let len = a.halt_offset;
    let bytes = if let Some(bytes) = shared {
        bytes
    } else {
        let Some(bytes) = archive::read_file(req, &fname, false)? else {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.runtime_exception,
                "Unable to read stub",
            ));
        };
        Arc::new(bytes)
    };
    if let Some(stub) = bytes.get(..len) {
        Ok(stub.to_vec())
    } else {
        let ce = Throwables::of(req)?;
        Err(throw_exception_ex(
            req,
            ce.runtime_exception,
            "Unable to read stub",
        ))
    }
}

/// `Phar::setAlias()`: the alias changed and the archive written, the old
/// one back where the write fails.
pub(crate) fn set_alias(req: &mut Request, id: ArchiveId, alias: &[u8]) -> Result<(), Thrown> {
    let Some(a) = archives(req).get(id) else {
        return Ok(());
    };
    let (is_data, is_tar, fname) = (a.is_data, a.is_tar(), lossy(&a.fname).into_owned());
    let same = a.alias.as_deref().unwrap_or_default() == alias;
    if readonly(req) && !is_data {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.unexpected_value_exception,
            "Cannot write out phar archive, phar is read-only",
        ));
    }
    archives(req).last = None;
    if is_data {
        let message = if is_tar {
            "A Phar alias cannot be set in a plain tar archive"
        } else {
            "A Phar alias cannot be set in a plain zip archive"
        };
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.unexpected_value_exception,
            message,
        ));
    }
    if same {
        return Ok(());
    }
    if let Some(holder) = archives(req).by_alias(alias) {
        let other = fname_of(req, holder);
        if !archive::free_alias(req, holder) {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.phar_exception,
                &format!(
                    "alias \"{}\" is already used for archive \"{}\" and cannot be used for other archives",
                    lossy(alias),
                    lossy(&other)
                ),
            ));
        }
    } else if !path::valid_alias(alias) {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.unexpected_value_exception,
            &format!(
                "Invalid alias \"{}\" specified for phar \"{fname}\"",
                lossy(alias)
            ),
        ));
    }
    let table = archives(req);
    let old = table.get(id).and_then(|a| a.alias.clone());
    let readd = old
        .as_deref()
        .is_some_and(|old| table.alias_map.remove(old).is_some());
    let Some(a) = table.get_mut(id) else {
        return Ok(());
    };
    let old_temp = a.is_temporary_alias;
    a.alias = (!alias.is_empty()).then(|| alias.to_vec());
    a.is_temporary_alias = false;
    if let Some(error) = write::flush(req, id)? {
        let table = archives(req);
        if let Some(a) = table.get_mut(id) {
            a.alias.clone_from(&old);
            a.is_temporary_alias = old_temp;
        }
        if readd && let Some(old) = old {
            table.alias_map.add(&old, id).ok();
        }
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(req, ce.phar_exception, &error));
    }
    archives(req).alias_map.add(alias, id).ok();
    Ok(())
}

/// The refusal of a stub for a data archive.
fn no_stub_in_data(req: &mut Request, id: ArchiveId) -> Result<(), Thrown> {
    let Some(a) = archives(req).get(id) else {
        return Ok(());
    };
    if !a.is_data {
        return Ok(());
    }
    let message = if a.is_tar() {
        "A Phar stub cannot be set in a plain tar archive"
    } else {
        "A Phar stub cannot be set in a plain zip archive"
    };
    let ce = Throwables::of(req)?;
    Err(throw_exception_ex(
        req,
        ce.unexpected_value_exception,
        message,
    ))
}

/// A stub written, its error as `PharException`.
fn flushed_with(
    req: &mut Request,
    id: ArchiveId,
    stub: Option<&[u8]>,
    is_default: bool,
) -> Result<(), Thrown> {
    match write::flush_ex(req, id, stub, is_default)? {
        Some(error) => {
            let ce = Throwables::of(req)?;
            Err(throw_exception_ex(req, ce.phar_exception, &error))
        }
        None => Ok(()),
    }
}

/// `Phar::setDefaultStub()`: the generated stub for a phar-format
/// archive, the tar and zip formats' own for theirs.
pub(crate) fn set_default_stub(
    req: &mut Request,
    id: ArchiveId,
    index: Option<&[u8]>,
    web: Option<&[u8]>,
) -> Result<(), Thrown> {
    no_stub_in_data(req, id)?;
    let tar_or_zip = archives(req)
        .get(id)
        .is_some_and(|a| a.is_tar() || a.is_zip());
    if (index.is_some() || web.is_some()) && tar_or_zip {
        let at = if index.is_some() { 1 } else { 2 };
        return Err(php_ext::raising::arg_value_error(
            req,
            at,
            "be null for a tar- or zip-based phar stub, string given",
        ));
    }
    if readonly(req) {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.unexpected_value_exception,
            "Cannot change stub: phar.readonly=1",
        ));
    }
    let stub = if tar_or_zip {
        None
    } else {
        match write::default_stub(index, web) {
            Ok(stub) => Some(stub),
            Err(error) => {
                let ce = Throwables::of(req)?;
                return Err(throw_exception_ex(
                    req,
                    ce.unexpected_value_exception,
                    &error,
                ));
            }
        }
    };
    flushed_with(req, id, stub.as_deref(), true)
}

/// `Phar::setStub()`: a string, or with a deprecation a stream read to
/// `$length` or its end, set as the stub.
pub(crate) fn set_stub(
    req: &mut Request,
    id: ArchiveId,
    stub: &Value,
    length: Passed<Raw<i64>>,
) -> Result<(), Thrown> {
    let is_data = archives(req).get(id).is_some_and(|a| a.is_data);
    if readonly(req) && !is_data {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.unexpected_value_exception,
            "Cannot change stub, phar is read-only",
        ));
    }
    no_stub_in_data(req, id)?;
    let count = req.frames().argument_count();
    let length = match length {
        Passed::Given(raw) => Some(raw.value()),
        Passed::Omitted => None,
    };
    let as_long = length.as_ref().map(|v| long_quiet(&v.deref()));
    let stream = Stream::from_value(req, &stub.deref());
    let resource = matches!(stub.deref().as_ref(), ValueRef::Resource(_));
    if resource && (1..=2).contains(&count) && as_long.is_none_or(|l| l.is_some()) {
        let name = req.frames().active_function_name();
        deprecated(
            req,
            &format!("Calling {name}(resource $stub, int $length) is deprecated"),
        );
        let Some(stream) = stream else {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.unexpected_value_exception,
                "Cannot change stub, unable to read from input stream",
            ));
        };
        let len = as_long.flatten().unwrap_or(-1);
        let max = usize::try_from(len).ok().filter(|l| *l > 0);
        let content = stream.copy_to_mem(req, max)?;
        let bytes = content.as_bytes().to_vec();
        content.give_up(req);
        return flushed_with(req, id, Some(&bytes), false);
    }
    arity(req, 1, Some(1))?;
    let bytes = match stub.deref().as_ref() {
        ValueRef::Str(s) => s.as_bytes().to_vec(),
        ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Bool(_) => {
            with_string(req, stub, <[u8]>::to_vec)?
        }
        _ => {
            let given = value_name(req, stub);
            return Err(php_ext::raising::arg_type_error(
                req,
                1,
                &[b"be of type string, ".as_slice(), &given, b" given"].concat(),
            ));
        }
    };
    flushed_with(req, id, Some(&bytes), false)
}

/// `l` read quietly: an int, a float that fits, or a numeric string; a
/// boolean or null as php's weak typing reads them.
fn long_quiet(value: &Value) -> Option<i64> {
    match value.as_ref() {
        ValueRef::Int(i) => Some(i),
        ValueRef::Bool(b) => Some(i64::from(b)),
        ValueRef::Null => Some(0),
        ValueRef::Float(f) if f.is_finite() && f.fract() == 0.0 => Some(f as i64),
        ValueRef::Str(s) => std::str::from_utf8(s.as_bytes()).ok()?.trim().parse().ok(),
        _ => None,
    }
}

/// `Phar::unlinkArchive()`: an archive nothing holds dropped and its file
/// removed.
pub(crate) fn unlink_archive(req: &mut Request, name: &[u8]) -> Result<(), Thrown> {
    let shown = lossy(name).into_owned();
    if name.is_empty() {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.phar_exception,
            "Unknown phar archive \"\"",
        ));
    }
    let id = match open::open_from_filename(req, name, None, true)? {
        Ok(id) => id,
        Err(error) => {
            let tail = error.map(|e| format!(": {e}")).unwrap_or_default();
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.phar_exception,
                &format!("Unknown phar archive \"{shown}\"{tail}"),
            ));
        }
    };
    if let Some(file) = req.frames().current_file().map(<[u8]>::to_vec)
        && file.len() >= 7
        && file[..7].eq_ignore_ascii_case(b"phar://")
        && let Ok(split) = path::split_fname(req, &file, path::Exec::Either, path::Create::No)?
        && split.arch == name
    {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.phar_exception,
            &format!("phar archive \"{shown}\" cannot be unlinked from within itself"),
        ));
    }
    if archives(req).get(id).is_some_and(|a| a.refcount != 0) {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.phar_exception,
            &format!(
                "phar archive \"{shown}\" has open file handles or objects.  fclose() all file handles, and unset() all objects prior to calling unlinkArchive()"
            ),
        ));
    }
    let fname = fname_of(req, id);
    archives(req).last = None;
    archive::delref(req, id);
    std::fs::remove_file(std::ffi::OsStr::from_bytes(&fname)).ok();
    Ok(())
}

// ---- `phar_build`

/// What one walk builds: the archive, the iterator's class for the
/// messages, the base directory, the map it answers and the buffer the
/// contents gather in (`pass.fp`).
struct Build {
    id: ArchiveId,
    class: String,
    base: Vec<u8>,
    ret: Owner<PhpArray>,
    buffer: Vec<u8>,
}

/// `Phar::buildFromDirectory()`: a `RecursiveIteratorIterator` over a
/// `RecursiveDirectoryIterator`, filtered by a `RegexIterator` where a
/// pattern is given, walked by [`build_one`].
pub(crate) fn from_directory(
    req: &mut Request,
    id: ArchiveId,
    dir: PhpString,
    pattern: PhpString,
) -> Result<Owner<PhpArray>, Thrown> {
    let rdi = class_named(req, b"RecursiveDirectoryIterator")?;
    let dir_value = Value::string(dir);
    let iter = php_ext::call::new_object(req, rdi)?;
    let constructed = construct_as(
        req,
        b"RecursiveDirectoryIterator",
        *iter,
        &[dir_value, Value::int(4096 | 8192)],
    );
    if let Err(thrown) = constructed {
        iter.give_up(req);
        return Err(thrown);
    }
    let rii = match class_named(req, b"RecursiveIteratorIterator") {
        Ok(rii) => rii,
        Err(thrown) => {
            iter.give_up(req);
            return Err(thrown);
        }
    };
    let iteriter = instantiate(req, rii, &[iter.as_value()], &[]);
    iter.give_up(req);
    let iteriter = iteriter?;
    let walked = if pattern.as_bytes().is_empty() {
        iteriter
    } else {
        let regex = match class_named(req, b"RegexIterator") {
            Ok(regex) => regex,
            Err(thrown) => {
                iteriter.give_up(req);
                return Err(thrown);
            }
        };
        let made = instantiate(
            req,
            regex,
            &[iteriter.as_value(), Value::string(pattern)],
            &[],
        );
        iteriter.give_up(req);
        made?
    };
    let dir = dir_bytes(&Value::string(dir));
    let out = walk(req, id, *walked, dir);
    walked.give_up(req);
    out
}

fn dir_bytes(value: &Value) -> Vec<u8> {
    value.as_str_bytes().map(<[u8]>::to_vec).unwrap_or_default()
}

/// `Phar::buildFromIterator()`.
pub(crate) fn from_iterator(
    req: &mut Request,
    id: ArchiveId,
    iterator: PhpObject,
    base: &[u8],
) -> Result<Owner<PhpArray>, Thrown> {
    walk(req, id, iterator, base.to_vec())
}

/// `spl_iterator_apply` with `phar_build`, the gathered contents the
/// archive's `ufp` and the archive written where every element went in.
fn walk(
    req: &mut Request,
    id: ArchiveId,
    iterator: PhpObject,
    base: Vec<u8>,
) -> Result<Owner<PhpArray>, Thrown> {
    let class = class_name(req, iterator);
    let ret = PhpArray::new(req)?;
    let mut build = Build {
        id,
        class,
        base,
        ret,
        buffer: Vec::new(),
    };
    let walked =
        php_ext_spl::iterator_apply(req, iterator, |req, it| build_one(req, &mut build, it));
    let Build { ret, buffer, .. } = build;
    if let Err(thrown) = walked {
        ret.give_up(req);
        return Err(thrown);
    }
    if let Some(a) = archives(req).get_mut(id) {
        a.ufp = Some(Arc::new(buffer));
    }
    match write::flush(req, id) {
        Ok(None) => Ok(ret),
        Ok(Some(error)) => {
            ret.give_up(req);
            let ce = Throwables::of(req)?;
            Err(throw_exception_ex(req, ce.phar_exception, &error))
        }
        Err(thrown) => {
            ret.give_up(req);
            Err(thrown)
        }
    }
}

/// A class's name, as the messages print it.
fn class_name(req: &mut Request, object: PhpObject) -> String {
    req.symbols()
        .class_decl(object.class())
        .map(|decl| lossy(&decl.name.0).into_owned())
        .unwrap_or_default()
}

/// The key the iterator gives, which must be a string.
fn string_key(req: &mut Request, b: &Build, it: php_ext::call::Iter) -> Result<Vec<u8>, Thrown> {
    let key = it.key(req)?;
    let bytes = key.deref().as_str_bytes().map(<[u8]>::to_vec);
    key.give_up(req);
    bytes.ok_or_else(|| {
        let ce = match Throwables::of(req) {
            Ok(ce) => ce,
            Err(fatal) => return fatal,
        };
        throw_exception_ex(
            req,
            ce.unexpected_value_exception,
            &format!(
                "Iterator {} returned an invalid key (must return a string)",
                b.class
            ),
        )
    })
}

/// `phar_build`: one element added, a file named by a string or an
/// `SplFileInfo`, or a stream under its key; `false` to stop.
#[allow(clippy::too_many_lines)] // php's one function, its refusals in its order
fn build_one(req: &mut Request, b: &mut Build, it: php_ext::call::Iter) -> Result<bool, Thrown> {
    let value = it.value(req)?;
    let current = value.deref();
    let bad =
        |req: &mut Request, class: ClassId, m: String| Err(throw_exception_ex(req, class, &m));
    let mut fname: Vec<u8>;
    let key: Vec<u8>;
    match current.as_ref() {
        ValueRef::Str(s) => {
            fname = s.as_bytes().to_vec();
            value.give_up(req);
        }
        ValueRef::Resource(_) => {
            let Some(stream) = Stream::from_value(req, &current) else {
                value.give_up(req);
                let ce = Throwables::of(req)?;
                return bad(
                    req,
                    ce.bad_method_call_exception,
                    format!("Iterator {} returned an invalid stream handle", b.class),
                );
            };
            let k = string_key(req, b, it);
            value.give_up(req);
            key = k?;
            return add_built(req, b, &key, stream, None, b"[stream]");
        }
        ValueRef::Object(object) => {
            let info = lookup_class(req, b"SplFileInfo")?;
            if !info.is_some_and(|info| object.instance_of(req, info)) {
                value.give_up(req);
                let ce = Throwables::of(req)?;
                return bad(
                    req,
                    ce.unexpected_value_exception,
                    format!(
                        "Iterator {} returned an invalid value (must return a string, a stream, or an SplFileInfo object)",
                        b.class
                    ),
                );
            }
            if b.base.is_empty() {
                value.give_up(req);
                let ce = Throwables::of(req)?;
                return bad(
                    req,
                    ce.bad_method_call_exception,
                    format!(
                        "Iterator {} returns an SplFileInfo object, so base directory must be specified",
                        b.class
                    ),
                );
            }
            let seen = spl::filesystem(req, object);
            value.give_up(req);
            let path = match seen {
                Some(Filesystem::Dir { path, entry }) => {
                    let mut name = path.unwrap_or_default();
                    name.push(b'/');
                    name.extend_from_slice(&entry);
                    if req
                        .url_stat(&name, false, false)?
                        .found()
                        .is_some_and(|sb| sb[2] & 0o170_000 == 0o040_000)
                    {
                        return Ok(true);
                    }
                    name
                }
                Some(Filesystem::File { file_name }) => file_name.unwrap_or_default(),
                None => Vec::new(),
            };
            let Some(expanded) = archive::expand_filepath(req, &path) else {
                let ce = Throwables::of(req)?;
                return bad(
                    req,
                    ce.unexpected_value_exception,
                    "Could not resolve file path".into(),
                );
            };
            fname = expanded;
        }
        _ => {
            value.give_up(req);
            let ce = Throwables::of(req)?;
            return bad(
                req,
                ce.unexpected_value_exception,
                format!(
                    "Iterator {} returned an invalid value (must return a string, a stream, or an SplFileInfo object)",
                    b.class
                ),
            );
        }
    }
    if b.base.is_empty() {
        key = string_key(req, b, it)?;
    } else {
        let Some(base) = archive::expand_filepath(req, &b.base) else {
            let ce = Throwables::of(req)?;
            return bad(
                req,
                ce.unexpected_value_exception,
                "Could not resolve file path".into(),
            );
        };
        let blen = base.len();
        let slash = |c: u8| c == b'/';
        let inside = fname.len() >= blen && fname[..blen] == base[..] && blen > 0 && {
            let at = blen - usize::from(slash(base[blen - 1]));
            fname.get(at).is_none_or(|c| slash(*c))
        };
        if !inside {
            let ce = Throwables::of(req)?;
            return bad(
                req,
                ce.unexpected_value_exception,
                format!(
                    "Iterator {} returned a path \"{}\" that is not in the base directory \"{}\"",
                    b.class,
                    lossy(&fname),
                    lossy(&base)
                ),
            );
        }
        if fname.len() == blen {
            return Ok(true);
        }
        let mut rest = &fname[blen..];
        if rest.first().is_some_and(|c| *c == b'/' || *c == b'\\') {
            rest = &rest[1..];
        }
        key = rest.to_vec();
    }
    if req
        .environment_mut()
        .check_open_basedir(&fname, true)
        .is_err()
    {
        let ce = Throwables::of(req)?;
        return bad(
            req,
            ce.unexpected_value_exception,
            format!(
                "Iterator {} returned a path \"{}\" that open_basedir prevents opening",
                b.class,
                lossy(&fname)
            ),
        );
    }
    let Some(stream) = req.open_stream_with(&fname, b"rb", php_ext::request::Opening::default())?
    else {
        let ce = Throwables::of(req)?;
        return bad(
            req,
            ce.unexpected_value_exception,
            format!(
                "Iterator {} returned a file that could not be opened \"{}\"",
                b.class,
                lossy(&fname)
            ),
        );
    };
    // `add_built` closes the stream it opened, and its owner with it.
    let opened =
        archive::expand_filepath(req, &fname).unwrap_or_else(|| std::mem::take(&mut fname));
    add_built(req, b, &key, *stream, Some(stream), &opened)
}

/// `phar_build` from `after_open_fp`: the entry created, its contents
/// appended to the walk's buffer, and the name mapped to what was opened;
/// `owned`, the owner of a stream the walk opened, closed after.
fn add_built(
    req: &mut Request,
    b: &mut Build,
    key: &[u8],
    source: Stream,
    owned: Option<Owner<Stream>>,
    opened: &[u8],
) -> Result<bool, Thrown> {
    let done = |req: &mut Request, owned: Option<Owner<Stream>>| {
        if let Some(stream) = owned {
            stream.free(req);
        }
    };
    if path::is_magic(key) {
        done(req, owned);
        return Ok(true);
    }
    let fname = fname_of(req, b.id);
    let data = match write::get_or_create(req, &fname, key, b"w+b", Dir::No, true) {
        Ok(Ok(data)) => data,
        Err(thrown) => {
            done(req, owned);
            return Err(thrown);
        }
        Ok(Err(error)) => {
            done(req, owned);
            let error = error.unwrap_or_default();
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.bad_method_call_exception,
                &format!("Entry {} cannot be created: {error}", lossy(key)),
            ));
        }
    };
    let offset = b.buffer.len();
    let bytes = match archive::copy_all(req, source) {
        Ok(bytes) => bytes,
        Err(thrown) => {
            done(req, owned);
            entry::delref(req, data);
            return Err(thrown);
        }
    };
    b.buffer.extend_from_slice(&bytes);
    let mode = source.stat(req).map(|sb| sb[2]);
    let umask = req.environment().umask();
    let mut replaced = None;
    if let Target::Slot(slot) = data.target
        && let Some(e) = archives(req)
            .get_mut(b.id)
            .and_then(|a| a.manifest.at_mut(slot))
    {
        replaced = Some(std::mem::replace(&mut e.content, Content::Ufp));
        e.offset = i64::try_from(offset).unwrap_or(i64::MAX);
        e.offset_abs = e.offset;
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        e.uncompressed_filesize = len;
        e.compressed_filesize = len;
        match mode {
            Some(mode) => e.flags = u32::try_from(mode).unwrap_or(0) & ENT_PERM_MASK,
            None => e.flags &= !umask,
        }
    }
    if let Some(Content::Tmp(Some(stream))) = replaced {
        stream.free(req);
    }
    done(req, owned);
    let v = string(req, opened)?;
    b.ret.set_str(req, key, v)?;
    entry::delref(req, data);
    Ok(true)
}
