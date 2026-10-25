//! `Phar::extractTo()`: `phar_extract_file` for each entry asked for,
//! the name kept under the destination as `virtual_file_ex` expands it
//! from `/`.

use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

use php_ext::abi::Union;
use php_ext::raising::{Thrown, throw_exception_ex};
use php_ext::request::{MAXPATHLEN, Opening, OwnedStream as _, Request, Resolve, Streams as _};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{PhpArray, PhpString};

use crate::archive::{ArchiveId, Content, ENT_COMPRESSION_MASK, ENT_PERM_MASK, archives, lossy};
use crate::entry::{self, Target};
use crate::path;

/// A path as the file system takes it, a relative one against the
/// request's working directory, as `VCWD_*` resolves it.
fn os(req: &Request, path: &[u8]) -> std::path::PathBuf {
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(path));
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        req.environment().cwd().join(path)
    }
}

/// `php_stream_mkdir(path, mode, PHP_STREAM_MKDIR_RECURSIVE)` on a plain
/// path.
fn mkdir_all(req: &Request, path: &[u8], mode: u32) -> bool {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(os(req, path))
        .is_ok()
}

/// `phar_extract_file`: one entry written under `dest`, its directory
/// made first; php's message where it cannot be.
#[allow(clippy::too_many_lines)] // php's one function, its refusals in its order
fn extract_file(
    req: &mut Request,
    id: ArchiveId,
    slot: usize,
    dest: &[u8],
    overwrite: bool,
) -> Result<Result<(), String>, Thrown> {
    let Some(e) = archives(req).get(id).and_then(|a| a.manifest.at(slot)) else {
        return Ok(Ok(()));
    };
    if e.is_mounted || path::is_magic(&e.filename) {
        return Ok(Ok(()));
    }
    let (name, is_dir, flags, compressed_fp) = (
        e.filename.clone(),
        e.is_dir,
        e.flags,
        matches!(e.content, Content::Fp) && e.flags & ENT_COMPRESSION_MASK != 0,
    );
    let shown = lossy(&name).into_owned();
    let expanded = req
        .environment_mut()
        .virtual_file_ex(b"/", &name, Resolve::Expand)
        .filter(|p| p.len() > 1);
    let Some(expanded) = expanded else {
        if name.len() > 50 {
            return Ok(Err(format!(
                "Cannot extract \"{}...\" to \"{}...\", extracted filename is too long for filesystem",
                lossy(&name[..50]),
                lossy(dest)
            )));
        }
        return Ok(Err(format!("Cannot extract \"{shown}\", internal error")));
    };
    let filename = &expanded[1..];
    let fullpath = [dest, b"/", filename].concat();
    if fullpath.len() >= MAXPATHLEN {
        let cut = lossy(&fullpath[..50]).into_owned();
        return Ok(Err(if name.len() > 50 {
            format!(
                "Cannot extract \"{}...\" to \"{cut}...\", extracted filename is too long for filesystem",
                lossy(&name[..50])
            )
        } else {
            format!(
                "Cannot extract \"{shown}\" to \"{cut}...\", extracted filename is too long for filesystem"
            )
        }));
    }
    let full = lossy(&fullpath).into_owned();
    if req
        .environment_mut()
        .check_open_basedir(&fullpath, true)
        .is_err()
    {
        return Ok(Err(format!(
            "Cannot extract \"{shown}\" to \"{full}\", openbasedir/safe mode restrictions in effect"
        )));
    }
    if !overwrite && req.url_stat(&fullpath, false, false)?.found().is_some() {
        return Ok(Err(format!(
            "Cannot extract \"{shown}\" to \"{full}\", path already exists"
        )));
    }
    let dir_end = match filename.iter().rposition(|b| *b == b'/') {
        Some(slash) => dest.len() + slash + 1,
        None => dest.len(),
    };
    let dir = &fullpath[..dir_end];
    if req.url_stat(dir, false, false)?.found().is_none() {
        let mode = if is_dir { flags & ENT_PERM_MASK } else { 0o777 };
        if !mkdir_all(req, dir, mode) {
            return Ok(Err(format!(
                "Cannot extract \"{shown}\", could not create directory \"{}\"",
                lossy(dir)
            )));
        }
    }
    if is_dir {
        return Ok(Ok(()));
    }
    let opening = Opening {
        warn: true,
        ..Opening::default()
    };
    let Some(out) = req.open_stream_with(&fullpath, b"w+b", opening)? else {
        return Ok(Err(format!(
            "Cannot extract \"{shown}\", could not open for writing \"{full}\""
        )));
    };
    let target = Target::Slot(slot);
    let read = |req: &mut Request| -> Result<Result<Vec<u8>, String>, Thrown> {
        if (compressed_fp || entry::efp(req, id, &target)?.is_none())
            && let Err(error) = entry::open_entry_fp(req, id, slot, true)?
        {
            return Ok(Err(error));
        }
        entry::contents(req, id, slot).map(Ok)
    };
    let bytes = match read(req) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            out.free(req);
            return Ok(Err(format!(
                "Cannot extract \"{shown}\" to \"{full}\", unable to open internal file pointer: {error}"
            )));
        }
        Err(thrown) => {
            out.free(req);
            return Err(thrown);
        }
    };
    let written = out.write(req, &bytes);
    out.free(req);
    if !bytes.is_empty() && written != Some(bytes.len()) {
        return Ok(Err(format!(
            "Cannot extract \"{shown}\" to \"{full}\", copying contents failed"
        )));
    }
    let mode = flags & ENT_PERM_MASK;
    if std::fs::set_permissions(os(req, &fullpath), std::fs::Permissions::from_mode(mode)).is_err()
    {
        return Ok(Err(format!(
            "Cannot extract \"{shown}\" to \"{full}\", setting file permissions failed"
        )));
    }
    Ok(Ok(()))
}

/// `extract_helper`: every entry, those under a name ending in `/`, or
/// the one named; how many were written, or php's message.
fn extract_matching(
    req: &mut Request,
    id: ArchiveId,
    search: Option<&[u8]>,
    dest: &[u8],
    overwrite: bool,
) -> Result<Result<usize, String>, Thrown> {
    let Some(a) = archives(req).get(id) else {
        return Ok(Ok(0));
    };
    let slots: Vec<usize> = match search {
        None => a.manifest.slots(),
        Some(search) if search.last() == Some(&b'/') => a
            .manifest
            .slots()
            .into_iter()
            .filter(|slot| {
                a.manifest
                    .key_at(*slot)
                    .is_some_and(|k| k.starts_with(search))
            })
            .collect(),
        Some(search) => match a.manifest.slot(search) {
            Some(slot) => vec![slot],
            None => return Ok(Ok(0)),
        },
    };
    let mut extracted = 0;
    for slot in slots {
        if let Err(error) = extract_file(req, id, slot, dest, overwrite)? {
            return Ok(Err(error));
        }
        extracted += 1;
    }
    Ok(Ok(extracted))
}

/// `Phar::extractTo()`.
#[allow(clippy::too_many_lines)] // php's one method, its refusals in its order
pub(crate) fn extract_to(
    req: &mut Request,
    id: ArchiveId,
    dest: &[u8],
    files: Option<Union<PhpArray, PhpString>>,
    overwrite: bool,
) -> Result<bool, Thrown> {
    let fname = archives(req)
        .get(id)
        .map(|a| a.fname.clone())
        .unwrap_or_default();
    let shown = lossy(&fname).into_owned();
    if crate::archive::read_file(req, &fname, false)?.is_none() {
        return Err(throw_exception_ex(
            req,
            "InvalidArgumentException",
            &format!("Invalid argument, {shown} cannot be found"),
        ));
    }
    if dest.is_empty() {
        return Err(throw_exception_ex(
            req,
            "InvalidArgumentException",
            "Invalid argument, extraction path must be non-zero length",
        ));
    }
    if dest.len() >= MAXPATHLEN {
        return Err(throw_exception_ex(
            req,
            "InvalidArgumentException",
            &format!(
                "Cannot extract to \"{}...\", destination directory is too long for filesystem",
                lossy(&dest[..50])
            ),
        ));
    }
    match req.url_stat(dest, false, false)?.found() {
        None => {
            if !mkdir_all(req, dest, 0o777) {
                return Err(throw_exception_ex(
                    req,
                    "RuntimeException",
                    &format!("Unable to create path \"{}\" for extraction", lossy(dest)),
                ));
            }
        }
        Some(sb) if sb[2] & 0o040_000 == 0 => {
            return Err(throw_exception_ex(
                req,
                "RuntimeException",
                &format!(
                    "Unable to use path \"{}\" for extraction, it is a file, must be a directory",
                    lossy(dest)
                ),
            ));
        }
        Some(_) => {}
    }
    let fail = |req: &mut Request, error: String| {
        Err(throw_exception_ex(
            req,
            "PharException",
            &format!("Extraction from phar \"{shown}\" failed: {error}"),
        ))
    };
    let missing = |req: &mut Request, name: &[u8]| {
        Err(throw_exception_ex(
            req,
            "PharException",
            &format!(
                "phar error: attempted to extract non-existent file or directory \"{}\" from phar \"{shown}\"",
                lossy(name)
            ),
        ))
    };
    match files {
        Some(Union::A(list)) => {
            if list.is_empty() {
                return Ok(false);
            }
            // `ZEND_HASH_FOREACH_VAL(files_ht, zval_file)`
            // (`Z/ext/phar/phar_object.c:4417`), no owner of the table
            // taken: the argument holds it while an extraction writes
            // through a stream wrapper.
            let walked = walk::foreach(req, list, Hold::None, |req, row| -> walk::Step<bool> {
                let value = row.value().deref();
                let Some(name) = value.deref().as_str_bytes().map(<[u8]>::to_vec) else {
                    return Err(throw_exception_ex(
                        req,
                        "InvalidArgumentException",
                        "Invalid argument, array of filenames to extract contains non-string value",
                    ));
                };
                match extract_matching(req, id, Some(&name), dest, overwrite)? {
                    Err(error) => return fail(req, error).map(ControlFlow::Break),
                    Ok(0) => return missing(req, &name).map(ControlFlow::Break),
                    Ok(_) => {}
                }
                Ok(ControlFlow::Continue(()))
            })?;
            Ok(walked.unwrap_or(true))
        }
        other => {
            let name = match other {
                Some(Union::B(name)) => Some(name.as_bytes().to_vec()),
                _ => None,
            };
            match extract_matching(req, id, name.as_deref(), dest, overwrite)? {
                Err(error) => fail(req, error),
                Ok(0) if name.is_some() => missing(req, name.as_deref().unwrap_or_default()),
                Ok(_) => Ok(true),
            }
        }
    }
}
