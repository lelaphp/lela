//! Paths inside and to archives: `phar_path_check.re`, which a path
//! inside an archive must pass; `phar_fix_filepath`, `.` and `..` taken
//! out; `phar_detect_phar_fname_ext` and `phar_split_fname`, which find
//! where the archive's name ends in `phar://archive.phar/inner/path`;
//! and the small checks of `phar_internal.h`.

use php_ext::raising::Thrown;
use php_ext::request::{Request, Streams as _};

use crate::archive::{self, archives, expand_filepath};

/// What `phar_path_check` found wrong, php's `pcr_err_*` with the words
/// its error carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathError {
    DoubleSlash,
    UpDir,
    CurrDir,
    BackSlash,
    Star,
    IllegalChar,
}

impl PathError {
    /// The words `phar error: invalid path "%s" contains %s` ends with.
    pub(crate) fn words(self) -> &'static str {
        match self {
            PathError::DoubleSlash => "double slash",
            PathError::UpDir => "upper directory reference",
            PathError::CurrDir => "current directory reference",
            PathError::BackSlash => "back-slash",
            PathError::Star => "star",
            PathError::IllegalChar => "illegal character",
        }
    }
}

/// The length of the UTF-8 sequence at the start of `s`, re2c's `UTF8`
/// rule with its first class `[\x1A-\x7F]`, or `None`.
fn utf8_len(s: &[u8]) -> Option<usize> {
    let tail =
        |from: usize, n: usize| (from..from + n).all(|i| matches!(s.get(i), Some(0x80..=0xBF)));
    let b1 = s.get(1).copied().unwrap_or(0);
    match s.first().copied()? {
        0x1A..=0x7F => Some(1),
        0xC2..=0xDF if tail(1, 1) => Some(2),
        0xE0 if (0xA0..=0xBF).contains(&b1) && tail(2, 1) => Some(3),
        0xE1..=0xEC | 0xEE..=0xEF if tail(1, 2) => Some(3),
        0xED if (0x80..=0x9F).contains(&b1) && tail(2, 1) => Some(3),
        0xF0 if (0x90..=0xBF).contains(&b1) && tail(2, 2) => Some(4),
        0xF1..=0xF3 if tail(1, 3) => Some(4),
        0xF4 if (0x80..=0x8F).contains(&b1) && tail(2, 2) => Some(4),
        _ => None,
    }
}

/// `phar_path_check` over `path`: the path as php leaves it, a leading
/// `/` taken off and, at a `?`, cut before it; or what is wrong with it.
/// The scan reads the C string, so it runs to the first NUL, and a NUL
/// before the end is an illegal character.
pub(crate) fn check(path: &[u8]) -> Result<Vec<u8>, PathError> {
    if path == b"." {
        return Err(PathError::CurrDir);
    }
    if path == b".." {
        return Err(PathError::UpDir);
    }
    let mut s = path.to_vec();
    s.push(0);
    let eos = |at: usize| matches!(s.get(at), Some(b'/' | 0));
    let mut p = 0;
    loop {
        let rest = &s[p..];
        if rest.starts_with(b"//") {
            return Err(PathError::DoubleSlash);
        }
        if rest.starts_with(b"/..") && eos(p + 3) {
            return Err(PathError::UpDir);
        }
        if rest.starts_with(b"/.") && eos(p + 2) {
            return Err(PathError::CurrDir);
        }
        match rest[0] {
            b'\\' => return Err(PathError::BackSlash),
            b'*' => return Err(PathError::Star),
            b'?' => {
                let start = usize::from(path.first() == Some(&b'/'));
                return Ok(path[start..p].to_vec());
            }
            0 => {
                let start = usize::from(path.first() == Some(&b'/'));
                if p != path.len() {
                    return Err(PathError::IllegalChar);
                }
                return Ok(path[start..].to_vec());
            }
            _ => match utf8_len(rest) {
                Some(n) => p += n,
                None => return Err(PathError::IllegalChar),
            },
        }
    }
}

/// `phar_validate_alias`: no `/`, `\`, `:`, `;`, newline or carriage
/// return.
pub(crate) fn valid_alias(alias: &[u8]) -> bool {
    !alias
        .iter()
        .any(|b| matches!(b, b'/' | b'\\' | b':' | b';' | b'\n' | b'\r'))
}

/// `phar_path_is_magic_phar_ex`: `.phar`, or a path in it, a leading `/`
/// allowed.
pub(crate) fn is_magic(path: &[u8]) -> bool {
    let path = path.strip_prefix(b"/").unwrap_or(path);
    path.starts_with(b".phar") && matches!(path.get(5), None | Some(b'/' | b'\\'))
}

/// `phar_fix_filepath`: `.` and `..` taken out of a path inside an
/// archive, which comes back with a leading `/`, and relative to `cwd`,
/// the directory of the file being included, where it starts `./`.
pub(crate) fn fix_filepath(path: &[u8], cwd: Option<&[u8]>) -> Vec<u8> {
    let mut out: Vec<u8> = match cwd {
        Some(cwd) if !cwd.is_empty() && path.len() > 2 && path.starts_with(b"./") => cwd.to_vec(),
        _ => b"/".to_vec(),
    };
    let body = path.strip_prefix(b"/").unwrap_or(path);
    let trimmed = &body[body.iter().take_while(|b| **b == b'/').count()..];
    if !trimmed.is_empty() && !trimmed.contains(&b'/') {
        if trimmed == b"." || trimmed == b".." {
            return b"/".to_vec();
        }
        return path.to_vec();
    }
    let parts: Vec<&[u8]> = body
        .split(|b| *b == b'/')
        .filter(|p| !p.is_empty())
        .collect();
    for part in parts {
        if part.len() >= 2 && part.iter().all(|b| *b == b'.') {
            while out.len() > 1 && out.last() != Some(&b'/') {
                out.pop();
            }
            if out.first() == Some(&b'/') && out.len() > 1 {
                out.pop();
            }
        } else if part != b"." {
            if out.len() > 1 {
                out.push(b'/');
            }
            out.extend_from_slice(part);
        }
    }
    out
}

/// Why no archive name was found in a path, `phar_detect_phar_fname_ext`'s
/// `FAILURE` with what it left in `ext_len`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoExt {
    /// Nothing that could be one.
    None,
    /// A URL, `scheme://`, `ext_len` of `-2`.
    Url,
    /// The first segment is an alias, `ext_len` of `-1`; the alias ends
    /// here.
    Alias(usize),
    /// An extension was found and did not pass, `ext_str` left at it.
    Rejected(usize, usize),
}

/// Which archives a name may be: `executable` is php's `1`, `0` a data
/// archive's, and `2` either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exec {
    Data,
    Executable,
    Either,
}

/// `for_create`: `0`, a name that must exist; `1`, one to create; `2`,
/// one that may exist or be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Create {
    No,
    Yes,
    Either,
}

/// `phar_analyze_path`: the archive name up to the extension's end, known
/// or there as a file where it must exist, or where it is to be created
/// a name whose directory is there.
fn analyze_path(
    req: &mut Request,
    fname: &[u8],
    end: usize,
    create: Create,
) -> Result<bool, Thrown> {
    let filename = &fname[..end];
    if let Some(real) = expand_filepath(req, filename)
        && archives(req).fname_map.contains(&real)
    {
        return Ok(true);
    }
    if let Some(sb) = req.url_stat(filename, false, false)?.found() {
        return Ok(sb[2] & S_IFDIR == 0 && create != Create::Yes);
    }
    if create == Create::No {
        return Ok(false);
    }
    let slash = filename.iter().rposition(|b| *b == b'/');
    let dir = &filename[..slash.unwrap_or(filename.len())];
    if let Some(sb) = req.url_stat(dir, false, false)?.found() {
        return Ok(sb[2] & S_IFDIR != 0);
    }
    if slash.is_some() {
        return Ok(false);
    }
    let Some(real) = expand_filepath(req, dir) else {
        return Ok(false);
    };
    let real = match find(&real, dir) {
        Some(at) => &real[..(at + end).min(real.len())],
        None => &real[..],
    };
    let Some(slash) = real.iter().rposition(|b| *b == b'/') else {
        return Ok(false);
    };
    Ok(req
        .url_stat(&real[..slash], false, false)?
        .found()
        .is_some_and(|sb| sb[2] & S_IFDIR != 0))
}

/// `S_IFDIR`, tested as php tests it, one bit of `st_mode`.
const S_IFDIR: i64 = 0o040_000;

/// `strstr`'s offset.
pub(crate) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `phar_check_str`: whether the extension at `at..at + len` of `fname`
/// names an archive of the kind asked for, the name analysed as a path.
fn check_str(
    req: &mut Request,
    fname: &[u8],
    at: usize,
    len: usize,
    exec: Exec,
    create: Create,
) -> Result<bool, Thrown> {
    if len >= 50 {
        return Ok(false);
    }
    let ext = &fname[at..];
    let cut = |i: usize| ext.get(i).copied().unwrap_or(0);
    if exec == Exec::Executable {
        let Some(pos) = find(ext, b".phar") else {
            return Ok(false);
        };
        let short = len.checked_sub(pos).is_some_and(|left| left < 5);
        if (pos != 0 && ext[pos - 1] == b'/') || short || !matches!(cut(pos + 5), 0 | b'/' | b'.') {
            return Ok(false);
        }
        return analyze_path(req, fname, at + len, create);
    }
    if exec == Exec::Data {
        let phar_like = find(ext, b".phar").is_some_and(|pos| {
            (pos == 0 || ext[pos - 1] != b'/') && matches!(cut(pos + 5), 0 | b'/' | b'.')
        });
        if !phar_like && !matches!(cut(1), b'.' | b'/' | 0) {
            return analyze_path(req, fname, at + len, create);
        }
        return Ok(false);
    }
    if !matches!(cut(1), b'.' | b'/' | 0) {
        return analyze_path(req, fname, at + len, create);
    }
    Ok(false)
}

/// `phar_detect_phar_fname_ext`: where the archive's extension stands in
/// `filename`, as `(offset, length)`, an open archive's name taking
/// precedence; `is_complete` is a name that is the archive's whole.
pub(crate) fn detect_ext(
    req: &mut Request,
    filename: &[u8],
    exec: Exec,
    create: Create,
    is_complete: bool,
) -> Result<Result<(usize, usize), NoExt>, Thrown> {
    if filename.len() <= 1 {
        return Ok(Err(NoExt::None));
    }
    if let Some(slash) = filename.iter().position(|b| *b == b'/')
        && slash != 0
    {
        if filename[slash - 1] == b':' && slash < filename.len() - 1 && filename[slash + 1] == b'/'
        {
            return Ok(Err(NoExt::Url));
        }
        if archives(req).alias_map.contains(&filename[..slash]) {
            return Ok(Err(NoExt::Alias(slash)));
        }
    }
    let judge = |a: &archive::Archive| match exec {
        Exec::Either => true,
        Exec::Executable => !a.is_data,
        Exec::Data => a.is_data,
    };
    let table = archives(req);
    if !table.fname_map.is_empty() {
        if is_complete {
            if let Some(a) = table.by_fname(filename).and_then(|id| table.get(id)) {
                let at = filename.len() - a.ext_len.min(filename.len());
                return Ok(if judge(a) {
                    Ok((at, a.ext_len))
                } else {
                    Err(NoExt::Rejected(at, a.ext_len))
                });
            }
        } else {
            for (key, id) in table.fname_map.iter() {
                if key.len() > filename.len() || !filename.starts_with(key) {
                    continue;
                }
                if filename.len() == key.len() || matches!(filename[key.len()], b'/' | 0) {
                    let Some(a) = table.get(*id) else { continue };
                    let at = key.len() - a.ext_len.min(key.len());
                    return Ok(if judge(a) {
                        Ok((at, a.ext_len))
                    } else {
                        Err(NoExt::Rejected(at, a.ext_len))
                    });
                }
            }
        }
    }
    let dot_from = |from: usize| {
        filename
            .get(from..)
            .and_then(|s| s.iter().position(|b| *b == b'.'))
            .map(|p| p + from)
    };
    let mut pos = dot_from(1);
    loop {
        let Some(mut at) = pos else {
            return Ok(Err(NoExt::None));
        };
        while at != 0 && matches!(filename[at - 1], b'/' | 0) {
            match dot_from(at + 1) {
                Some(next) => at = next,
                None => return Ok(Err(NoExt::None)),
            }
        }
        let slash = filename[at..]
            .iter()
            .position(|b| *b == b'/')
            .map(|s| s + at);
        let Some(slash) = slash else {
            let len = filename[at..]
                .iter()
                .position(|b| *b == 0)
                .unwrap_or(filename.len() - at);
            return Ok(if check_str(req, filename, at, len, exec, create)? {
                Ok((at, len))
            } else {
                Err(NoExt::Rejected(at, len))
            });
        };
        if check_str(req, filename, at, slash - at, exec, create)? {
            return Ok(Ok((at, slash - at)));
        }
        pos = dot_from(at + 1);
        if pos.is_none() {
            return Ok(Err(NoExt::Rejected(at, slash - at)));
        }
    }
}

/// What `phar_split_fname` found: the archive's name and the path inside
/// it, `/` for none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Split {
    pub(crate) arch: Vec<u8>,
    pub(crate) entry: Vec<u8>,
}

/// Why `phar_split_fname` failed: `arch` is php's `*arch` restored for
/// the message where no `/` was found after the extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoSplit {
    pub(crate) arch: Option<Vec<u8>>,
}

/// `phar_split_fname`: `phar://` taken off, the archive's name found, and
/// the rest normalised by [`fix_filepath`].
pub(crate) fn split_fname(
    req: &mut Request,
    filename: &[u8],
    exec: Exec,
    create: Create,
) -> Result<Result<Split, NoSplit>, Thrown> {
    if filename.contains(&0) {
        return Ok(Err(NoSplit { arch: None }));
    }
    let filename = if filename.len() >= 7 && filename[..7].eq_ignore_ascii_case(b"phar://") {
        &filename[7..]
    } else {
        filename
    };
    let (at, len) = match detect_ext(req, filename, exec, create, false)? {
        Ok(found) => found,
        Err(NoExt::Alias(end)) => (end, 0),
        Err(NoExt::None | NoExt::Url) => {
            return Ok(Err(NoSplit {
                arch: Some(filename.to_vec()),
            }));
        }
        Err(NoExt::Rejected(..)) => return Ok(Err(NoSplit { arch: None })),
    };
    let arch_len = at + len;
    let arch = filename[..arch_len].to_vec();
    let entry = if arch_len < filename.len() {
        fix_filepath(&filename[arch_len..], None)
    } else {
        b"/".to_vec()
    };
    Ok(Ok(Split { arch, entry }))
}

#[cfg(test)]
mod tests {
    use super::{PathError, check, fix_filepath, is_magic, valid_alias};

    #[test]
    fn path_check_is_phar_path_check_re() {
        assert_eq!(check(b"/a/b.php"), Ok(b"a/b.php".to_vec()));
        assert_eq!(check(b"./a"), Ok(b"./a".to_vec()));
        assert_eq!(check(b"a//b"), Err(PathError::DoubleSlash));
        assert_eq!(check(b"a/../b"), Err(PathError::UpDir));
        assert_eq!(check(b"a/."), Err(PathError::CurrDir));
        assert_eq!(check(b"a\\b"), Err(PathError::BackSlash));
        assert_eq!(check(b"a*"), Err(PathError::Star));
        assert_eq!(check(b"/a?q=1"), Ok(b"a".to_vec()));
        assert_eq!(check(b"a\x01"), Err(PathError::IllegalChar));
        assert_eq!(check(b"a\0b"), Err(PathError::IllegalChar));
        assert_eq!(check("é".as_bytes()), Ok("é".as_bytes().to_vec()));
        assert_eq!(check(b"\xff"), Err(PathError::IllegalChar));
    }

    #[test]
    fn fix_filepath_takes_out_dots() {
        assert_eq!(fix_filepath(b"/a/./b/../c", None), b"/a/c".to_vec());
        assert_eq!(fix_filepath(b"/a", None), b"/a".to_vec());
        assert_eq!(fix_filepath(b"/..", None), b"/".to_vec());
        assert_eq!(fix_filepath(b"/a/b/", None), b"/a/b".to_vec());
        assert_eq!(fix_filepath(b"./x/y", Some(b"sub")), b"sub/x/y".to_vec());
        assert!(is_magic(b"/.phar/stub.php") && is_magic(b".phar") && !is_magic(b".phar2"));
        assert!(valid_alias(b"hio") && !valid_alias(b"a:b"));
    }
}
