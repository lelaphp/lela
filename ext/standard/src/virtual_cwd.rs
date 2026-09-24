//! `Z/Zend/zend_virtual_cwd.c`'s path resolution, `virtual_file_ex` over
//! `tsrm_realpath_r`, which `expand_filepath` (`Z/main/fopen_wrappers.c:787`)
//! and `VCWD_REALPATH` reach: a path made absolute against the request's
//! working directory, `.` and `..` taken out, and, where the file is
//! there, each symbolic link on the way replaced by what it points at.
//! The realpath cache is left out: nothing a program sees depends on it
//! here, since a request reads its paths fresh. The separator is `/`
//! alone and `MAXPATHLEN` is the host's 1024.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// `MAXPATHLEN` on this host.
pub(super) const MAXPATHLEN: usize = 1024;

/// `LINK_MAX` as `zend_virtual_cwd.c` redefines it: how many links one
/// resolution follows before it gives up.
const LINK_MAX: i32 = 32;

/// `CWD_EXPAND`, `CWD_FILEPATH` and `CWD_REALPATH`: expand `.` and `..`
/// only; resolve links where the file is there and expand otherwise;
/// resolve links and fail where the file is not there.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Resolve {
    Expand,
    FilePath,
    RealPath,
}

/// The C buffer `tsrm_realpath_r` works in place on: a read past what
/// is written is the NUL php's buffer holds there, and a write past it
/// grows it.
struct Buf(Vec<u8>);

impl Buf {
    fn at(&self, i: usize) -> u8 {
        self.0.get(i).copied().unwrap_or(0)
    }

    fn put(&mut self, i: usize, byte: u8) {
        if self.0.len() <= i {
            self.0.resize(i + 1, 0);
        }
        if let Some(slot) = self.0.get_mut(i) {
            *slot = byte;
        }
    }

    fn write(&mut self, at: usize, bytes: &[u8]) {
        for (k, &byte) in bytes.iter().enumerate() {
            self.put(at + k, byte);
        }
    }

    fn head(&self, len: usize) -> Vec<u8> {
        (0..len).map(|i| self.at(i)).collect()
    }
}

fn host(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

/// `tsrm_realpath_r` (`Z/Zend/zend_virtual_cwd.c:488`), the Unix half:
/// the path in `p` up to `len` resolved from its last component back,
/// the part before `start` kept as it is; the new length, or `None` for
/// php's `(size_t)-1`.
#[allow(clippy::too_many_lines)] // one C function, kept whole
fn realpath_r(
    p: &mut Buf,
    start: usize,
    mut len: usize,
    ll: &mut i32,
    how: Resolve,
    mut is_dir: bool,
    mut link_is_dir: Option<&mut bool>,
) -> Option<usize> {
    loop {
        if len <= start {
            if let Some(d) = link_is_dir.as_deref_mut() {
                *d = true;
            }
            return Some(start);
        }
        let mut i = len;
        while i > start && p.at(i - 1) != b'/' {
            i -= 1;
        }
        if i == len || (i + 1 == len && p.at(i) == b'.') {
            // A doubled slash or a `.`.
            len = i.saturating_sub(1);
            is_dir = true;
            continue;
        }
        if i + 2 == len && p.at(i) == b'.' && p.at(i + 1) == b'.' {
            // A `..` and the directory before it.
            if let Some(d) = link_is_dir.as_deref_mut() {
                *d = true;
            }
            if i <= start + 1 {
                return Some(if start > 0 { start } else { len });
            }
            return match realpath_r(p, start, i - 1, ll, how, true, None) {
                Some(mut j) if j > start => {
                    j -= 1;
                    while j > start && p.at(j) != b'/' {
                        j -= 1;
                    }
                    if start == 0 {
                        // A leading `..` of a relative path stays.
                        if j == 0 && p.at(0) == b'.' && p.at(1) == b'.' && p.at(2) == b'/' {
                            p.write(3, b"../");
                            j = 5;
                        } else if j > 0
                            && p.at(j + 1) == b'.'
                            && p.at(j + 2) == b'.'
                            && p.at(j + 3) == b'/'
                        {
                            j += 4;
                            p.write(j, b"../");
                            j += 2;
                        }
                    }
                    Some(j)
                }
                Some(0) if start == 0 => {
                    p.write(0, b"../");
                    Some(2)
                }
                other => other,
            };
        }
        let tmp = p.head(len);
        let mut save = how != Resolve::Expand;
        let meta = if save {
            std::fs::symlink_metadata(host(&tmp)).ok()
        } else {
            None
        };
        if save && meta.is_none() {
            if how == Resolve::RealPath {
                return None;
            }
            save = false;
        }
        let mut directory = false;
        if meta.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
            *ll += 1;
            if *ll > LINK_MAX {
                return None;
            }
            let target = std::fs::read_link(host(&tmp)).ok()?;
            let target = target.as_os_str().as_bytes();
            let target = target.get(..MAXPATHLEN).unwrap_or(target).to_vec();
            let j = if target.first() == Some(&b'/') {
                p.write(0, &target);
                realpath_r(p, 1, target.len(), ll, how, is_dir, Some(&mut directory))?
            } else {
                if i + target.len() >= MAXPATHLEN - 1 {
                    return None;
                }
                p.write(i, &target);
                p.write(0, tmp.get(..i - 1).unwrap_or_default());
                p.put(i - 1, b'/');
                realpath_r(
                    p,
                    start,
                    i + target.len(),
                    ll,
                    how,
                    is_dir,
                    Some(&mut directory),
                )?
            };
            if let Some(d) = link_is_dir.as_deref_mut() {
                *d = directory;
            }
            return Some(j);
        }
        if save {
            directory = meta.as_ref().is_some_and(std::fs::Metadata::is_dir);
            if let Some(d) = link_is_dir.as_deref_mut() {
                *d = directory;
            }
            if is_dir && !directory {
                return None;
            }
        }
        let j = if i <= start + 1 {
            start
        } else {
            // Some leading directories may be inaccessible.
            let prefix = if save { Resolve::FilePath } else { how };
            let j = realpath_r(p, start, i - 1, ll, prefix, true, None)?;
            if j > start {
                p.put(j, b'/');
                j + 1
            } else {
                j
            }
        };
        if j + len >= MAXPATHLEN - 1 + i {
            return None;
        }
        p.write(j, tmp.get(i..len).unwrap_or_default());
        return Some(j + (len - i));
    }
}

/// `virtual_file_ex(state, path, NULL, how)` (`Z/Zend/zend_virtual_cwd.c:1007`)
/// with `state` the directory `cwd`: the path resolved, or `None` where
/// php answers `1`, an empty path or one of `MAXPATHLEN` or more among
/// them.
pub(super) fn virtual_file_ex(cwd: &[u8], path: &[u8], how: Resolve) -> Option<Vec<u8>> {
    if path.is_empty() || path.len() >= MAXPATHLEN - 1 {
        return None;
    }
    let mut start = 1;
    let resolved = if path.first() == Some(&b'/') {
        path.to_vec()
    } else if cwd.is_empty() {
        start = 0;
        path.to_vec()
    } else {
        if path.len() + cwd.len() + 1 >= MAXPATHLEN - 1 {
            return None;
        }
        let mut joined = cwd.to_vec();
        if cwd.last() != Some(&b'/') {
            joined.push(b'/');
        }
        joined.extend_from_slice(path);
        joined
    };
    let add_slash = how != Resolve::RealPath && resolved.last() == Some(&b'/');
    let length = resolved.len();
    let mut buf = Buf(resolved);
    let mut ll = 0;
    let mut len = realpath_r(&mut buf, start, length, &mut ll, how, false, None)?;
    if start == 0 && len == 0 {
        buf.put(0, b'.');
        len = 1;
    }
    if add_slash && len > 0 && buf.at(len - 1) != b'/' {
        if len >= MAXPATHLEN - 1 {
            return None;
        }
        buf.put(len, b'/');
        len += 1;
    }
    Some(buf.head(len))
}

/// `expand_filepath_ex(path, real, relative_to, len)` under
/// `CWD_FILEPATH` (`Z/main/fopen_wrappers.c:787`): an absolute path as
/// it is, a relative one against `relative_to`, the working directory
/// the caller hands it; `None` for the empty path and wherever
/// [`virtual_file_ex`] fails.
pub(super) fn expand_filepath(path: &[u8], relative_to: &[u8]) -> Option<Vec<u8>> {
    if path.is_empty() {
        return None;
    }
    let cwd: &[u8] = if path.first() == Some(&b'/') {
        b""
    } else {
        relative_to
    };
    virtual_file_ex(cwd, path, Resolve::FilePath)
}
