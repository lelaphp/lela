//! `ZipArchive::extractTo` (`php_zip_extract_file`): each entry written
//! under the destination by its name made relative, directories made as
//! needed, the file's time set to the entry's.

use php_ext::abi::Union;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{MAXPATHLEN, OwnedStream as _, Request, Resolve, Streams as _, WrapperOp};
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, ValueRef};

use crate::archive::FL_UNCHANGED;
use crate::object::{Intern, basedir_refuses};

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

/// `php_zip_extract_file`: true past the read whatever it met, as php
/// answers `n < 0` over `zip_fclose`'s `ZIP_ER_*` code, which is never
/// negative.
///
/// ```php
/// $z = new ZipArchive; $z->open('extract-witness.zip', ZipArchive::CREATE|ZipArchive::OVERWRITE);
/// $z->addFromString('a', str_repeat('x', 195000)); $z->setCompressionName('a', ZipArchive::CM_STORE); $z->addFromString('b', 'bee'); $z->close();
/// $z->open('extract-witness.zip'); $h = fopen('extract-witness.zip', 'r+'); ftruncate($h, 5000); fclose($h);
/// @mkdir('extract-witness'); var_dump(@$z->extractTo('extract-witness', 'a')); echo filesize('extract-witness/a'), "\n";
/// unlink('extract-witness/a'); rmdir('extract-witness'); $z->close(); unlink('extract-witness.zip');
/// ```
/// ```expect
/// bool(true)
/// 4969
/// ```
fn extract_file(
    req: &mut Request,
    object: PhpObject,
    dest: &[u8],
    file: &[u8],
    idx: Option<usize>,
) -> Result<bool, Thrown> {
    let file = php_ext::coerce::c_str(file);
    let Some(archive) = object
        .native_mut::<Intern>(req)
        .and_then(|i| i.za.as_deref_mut())
    else {
        return Ok(false);
    };
    let idx = match idx {
        Some(idx) => idx,
        None => match archive.name_locate(file, 0) {
            Ok(idx) => idx,
            Err(_) => return Ok(false),
        },
    };
    let Some(cleaned) = req.virtual_file_ex(b"", file, Resolve::Expand) else {
        return Ok(false);
    };
    let Some(path) = make_relative(&cleaned).map(<[u8]>::to_vec) else {
        return Ok(false);
    };
    let Some(archive) = object
        .native_mut::<Intern>(req)
        .and_then(|i| i.za.as_deref_mut())
    else {
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
    let id = object.native::<Intern>(req).map_or(0, |i| i.id);
    loop {
        let mut file = crate::object::file(req, id);
        let chunk = zf.read_through(8192, &mut file);
        crate::object::file_back(req, id, file);
        match chunk {
            Ok(chunk) if !chunk.is_empty() => {
                stream.write(req, &chunk);
            }
            _ => break,
        }
    }
    let touched = req.wrapper_op(&full, WrapperOp::Touch(Some((st.mtime, st.mtime))));
    stream.free(req);
    touched?;
    Ok(true)
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
            let n = list.len();
            if n == 0 {
                return Ok(false);
            }
            for i in 0..n {
                let Some(v) = list
                    .get(&Key::Int(i64::try_from(i).unwrap_or(i64::MAX)))
                    .map(|v| v.deref())
                else {
                    continue;
                };
                if let ValueRef::Str(s) = v.as_ref() {
                    let name = s.as_bytes().to_vec();
                    if !extract_file(req, object, pathto, &name, None)? {
                        return Ok(false);
                    }
                }
            }
            Ok(true)
        }
        None => {
            let count = object
                .native::<Intern>(req)
                .and_then(|i| i.za.as_ref())
                .map_or(0, |za| za.num_entries());
            for i in 0..count {
                let name = object
                    .native_mut::<Intern>(req)
                    .and_then(|i| i.za.as_deref_mut())
                    .and_then(|za| za.get_name(i, FL_UNCHANGED).ok());
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
