//! `ZipArchive::extractTo` (`php_zip_extract_file`): each entry written
//! under the destination by its name made relative, directories made as
//! needed, the file's time set to the entry's.

use std::ops::ControlFlow;

use php_ext::abi::Union;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{
    MAXPATHLEN, OwnedStream as _, Request, Resolve, Stream, Streams as _, WrapperOp,
};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, ValueRef};

use crate::archive::FL_UNCHANGED;
use crate::holder::{self, ArchiveId};
use crate::object::{archive_id, basedir_refuses, za};
use crate::read::Reader;

/// `php_zip_make_relative_path`: an absolute path less its root, a
/// relative one from past its last `./`.
fn make_relative(path: &[u8]) -> Option<&[u8]> {
    if path.is_empty() {
        return None;
    }
    if path[0] == b'/' {
        return Some(&path[1..]);
    }
    let mut i = path.len();
    loop {
        while i > 0 && path.get(i) != Some(&b'/') {
            i -= 1;
        }
        if i == 0 {
            return Some(path);
        }
        if i >= 2 && path[i - 1] == b'.' {
            return Some(&path[i + 1..]);
        }
        i -= 1;
    }
}

/// `zend_dirname` of a relative path: up to its last `/`, `.` for none.
fn dirname(path: &[u8]) -> &[u8] {
    let end = path.iter().rposition(|b| *b != b'/').map_or(0, |e| e + 1);
    let path = &path[..end];
    match path.iter().rposition(|b| *b == b'/') {
        Some(at) => {
            let d = &path[..at];
            let d_end = d.iter().rposition(|b| *b != b'/').map_or(0, |e| e + 1);
            if d_end == 0 { b"/" } else { &d[..d_end] }
        }
        None => b".",
    }
}

fn join(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = a.to_vec();
    out.push(b'/');
    out.extend_from_slice(b);
    out
}

/// `php_zip_extract_file`: false where the read or the write fails,
/// after php 8.5.12's warning for the entry's error, a CRC error among
/// them.
///
/// ```php
/// $z = new ZipArchive; $z->open('extract-witness.zip', ZipArchive::CREATE|ZipArchive::OVERWRITE);
/// $z->addFromString('a', str_repeat('x', 195000)); $z->setCompressionName('a', ZipArchive::CM_STORE); $z->addFromString('b', 'bee'); $z->close();
/// $z->open('extract-witness.zip'); $h = fopen('extract-witness.zip', 'r+'); ftruncate($h, 5000); fclose($h);
/// @mkdir('extract-witness'); var_dump($z->extractTo('extract-witness', 'a')); echo filesize('extract-witness/a'), "\n";
/// $z->close(); $z->open('extract-witness.zip', ZipArchive::CREATE|ZipArchive::OVERWRITE); $z->addFromString('c', str_repeat('y', 70)); $z->setCompressionName('c', ZipArchive::CM_STORE); $z->close();
/// $b = file_get_contents('extract-witness.zip'); $b[strpos($b, 'yyyy') + 2] = 'Z'; file_put_contents('extract-witness.zip', $b);
/// $z->open('extract-witness.zip'); var_dump($z->extractTo('extract-witness', 'c'), filesize('extract-witness/c'), $z->extractTo('extract-witness', [5 => 'c']));
/// unlink('extract-witness/a'); unlink('extract-witness/c'); rmdir('extract-witness'); $z->close(); unlink('extract-witness.zip');
/// ```
/// ```expect
///
/// Warning: ZipArchive::extractTo(): Cannot extract "a": "Premature end of file" in %s on line %d
/// bool(false)
/// 4969
///
/// Warning: ZipArchive::extractTo(): Cannot extract "c": "CRC error" in %s on line %d
///
/// Warning: ZipArchive::extractTo(): Cannot extract "c": "CRC error" in %s on line %d
/// bool(false)
/// int(70)
/// bool(false)
/// ```
fn extract_file(
    req: &mut Request,
    object: PhpObject,
    dest: &[u8],
    file: &[u8],
    idx: Option<usize>,
) -> Result<bool, Thrown> {
    let file = php_ext::coerce::c_str(file);
    let Some(archive) = za(req, object) else {
        return Ok(false);
    };
    let idx = match idx {
        Some(idx) => idx,
        None => match archive.name_locate(file, 0) {
            Ok(idx) => idx,
            Err(_) => return Ok(false),
        },
    };
    let Some(cleaned) = req
        .environment_mut()
        .virtual_file_ex(b"", file, Resolve::Expand)
    else {
        return Ok(false);
    };
    let Some(path) = make_relative(&cleaned).map(<[u8]>::to_vec) else {
        return Ok(false);
    };
    let Some(archive) = za(req, object) else {
        return Ok(false);
    };
    let Ok(st) = archive.stat_index(idx, 0) else {
        return Ok(false);
    };
    if path.len() >= MAXPATHLEN {
        return Ok(false);
    }
    let dir_only = path.len() > 1 && path.last() == Some(&b'/');
    let (dir_full, base) = if dir_only {
        (join(dest, &path), Vec::new())
    } else {
        let d = dirname(&path);
        let full = if d.is_empty() || d == b"." {
            dest.to_vec()
        } else {
            join(dest, d)
        };
        if basedir_refuses(req, &full) {
            return Ok(false);
        }
        (full, php_ext_standard::php_basename(&path, b"").to_vec())
    };
    if req.url_stat_quiet(&dir_full, false)?.found().is_none() {
        let made = req.wrapper_op(
            &dir_full,
            WrapperOp::Mkdir {
                mode: 0o777,
                recursive: true,
            },
        )?;
        if !made {
            return Ok(false);
        }
    }
    if dir_only {
        return Ok(true);
    }
    let full = join(&dir_full, &base);
    if full.len() > MAXPATHLEN {
        docref(
            req,
            Level::Warning,
            &format!("Full extraction path exceed MAXPATHLEN ({MAXPATHLEN})"),
        );
        return Ok(false);
    }
    if basedir_refuses(req, &full) {
        return Ok(false);
    }
    let Some(Ok(mut zf)) = crate::object::with_file(req, object, |za| za.fopen(idx, 0, None))
    else {
        return Ok(false);
    };
    let Some(stream) = req.open_stream(&full, b"w+b", false)? else {
        return Ok(false);
    };
    let Some(id) = archive_id(req, object) else {
        stream.free(req);
        return Ok(false);
    };
    if !copy(req, id, &mut zf, &stream, file) {
        stream.free(req);
        return Ok(false);
    }
    let touched = req.wrapper_op(&full, WrapperOp::Touch(Some((st.mtime, st.mtime))));
    stream.free(req);
    touched?;
    Ok(true)
}

/// `php_zip_extract_file`'s copy: the entry read into `stream` 8192
/// bytes at a time to its end; false at a short write, and at a failed
/// read after php 8.5.12's warning for the entry's error.
fn copy(
    req: &mut Request,
    id: ArchiveId,
    zf: &mut Reader,
    stream: &Owner<Stream>,
    file: &[u8],
) -> bool {
    loop {
        match holder::read(req, id, zf, 8192) {
            Ok(chunk) if chunk.is_empty() => return true,
            Ok(chunk) => {
                if stream.write(req, &chunk) != Some(chunk.len()) {
                    return false;
                }
            }
            Err(status) => {
                let message = status.text(req);
                let name = String::from_utf8_lossy(file);
                let text = format!("Cannot extract \"{name}\": \"{message}\"");
                docref(req, Level::Warning, &text);
                return false;
            }
        }
    }
}

/// `ZipArchive::extractTo`'s body past `ZIP_FROM_OBJECT`.
pub(crate) fn extract_to(
    req: &mut Request,
    object: PhpObject,
    pathto: &[u8],
    files: Option<Union<PhpArray, PhpString>>,
) -> Result<bool, Thrown> {
    if pathto.is_empty() {
        return Ok(false);
    }
    if req.url_stat_quiet(pathto, false)?.found().is_none()
        && !req.wrapper_op(
            pathto,
            WrapperOp::Mkdir {
                mode: 0o777,
                recursive: true,
            },
        )?
    {
        return Ok(false);
    }
    match files {
        Some(Union::B(file)) => extract_file(req, object, pathto, file.as_bytes(), None),
        Some(Union::A(list)) => {
            if list.is_empty() {
                return Ok(false);
            }
            // `ZEND_HASH_FOREACH_VAL` (php 8.5.12): every name in the
            // array's order whatever its key, no owner of the table taken.
            let walked = walk::foreach(req, list, Hold::None, |req, row| -> walk::Step<()> {
                let name = match row.value().deref().as_ref() {
                    ValueRef::Str(s) => s.as_bytes().to_vec(),
                    _ => return Ok(ControlFlow::Continue(())),
                };
                if extract_file(req, object, pathto, &name, None)? {
                    Ok(ControlFlow::Continue(()))
                } else {
                    Ok(ControlFlow::Break(()))
                }
            })?;
            Ok(walked.is_none())
        }
        None => {
            let count = za(req, object).map_or(0, |za| za.num_entries());
            for i in 0..count {
                let name = za(req, object).and_then(|za| za.get_name(i, FL_UNCHANGED).ok());
                let Some(name) = name else {
                    return Ok(false);
                };
                if !extract_file(req, object, pathto, &name, Some(i))? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
    }
}
