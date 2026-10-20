//! The paths area of a request: a path made absolute against the
//! request's working directory, `realpath()` through `CWDG(realpath_cache)`
//! and the cache's listing and clearing, and how a path names a stream
//! wrapper. The cache is the engine's (`crate::virtual_cwd`), which every
//! request reads and enters.

use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use super::Environment;
use super::{Request, Resolve};

/// One bucket of the realpath cache, `realpath_cache_bucket` as
/// `realpath_cache_get()` shows it, with the bytes `realpath_cache_size()`
/// counts it as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealpathEntry {
    pub path: Vec<u8>,
    pub realpath: Vec<u8>,
    pub key: u64,
    pub is_dir: bool,
    pub expires: i64,
    pub size: i64,
}

impl Request {
    /// `php_request_startup`'s `Disable realpath cache if an open_basedir
    /// is set` (`Z/main/main.c:1918`), once the host's ini
    /// is applied: an `open_basedir` the request starts under turns the
    /// engine's cache off, for this request and every one after it, as
    /// php's process global stays off.
    pub(crate) fn realpath_cache_startup(&mut self) {
        if self
            .ini()
            .get(b"open_basedir")
            .is_some_and(|list| !list.is_empty())
        {
            self.shared.realpath_cache.turn_off();
        }
    }
}

impl Environment<&Request> {
    /// A path made absolute against the request's `cwd`.
    #[must_use]
    pub fn absolutize(self, path: &[u8]) -> PathBuf {
        let given = std::path::Path::new(std::ffi::OsStr::from_bytes(path));
        if given.is_absolute() {
            given.to_path_buf()
        } else {
            self.0.cwd.join(given)
        }
    }

    /// `CWDG(realpath_cache)` as `realpath_cache_get()` walks it, slot by
    /// slot and each chain newest first: each bucket's path as it was
    /// written, what it resolved to, its key, whether it is a directory,
    /// when it expires, and the bytes `realpath_cache_size()` counts it as.
    #[must_use]
    pub fn realpath_cache(self) -> Vec<RealpathEntry> {
        self.0.shared.realpath_cache.listed(|bucket| RealpathEntry {
            path: bucket.path.clone(),
            realpath: bucket.realpath.clone(),
            key: bucket.key,
            is_dir: bucket.is_dir,
            expires: bucket.expires,
            size: bucket.size(),
        })
    }
}

impl Environment<&mut Request> {
    /// `VCWD_REALPATH`, `tsrm_realpath`: the path against the request's
    /// working directory with every link resolved, through the realpath
    /// cache; `None` where the path does not exist.
    pub fn realpath(self, path: &[u8]) -> Option<PathBuf> {
        let cwd = self.0.cwd.as_os_str().as_encoded_bytes().to_vec();
        let real = self
            .0
            .environment_mut()
            .virtual_file_ex(&cwd, path, Resolve::RealPath)?;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(&real)))
    }

    /// `virtual_file_ex(state, path, NULL, how)` (`Z/Zend/zend_virtual_cwd.c:1007`)
    /// with `state` the directory `cwd`: `path` made absolute against it
    /// where it is relative (and left relative where `cwd` is empty),
    /// `.` and `..` taken out, and under [`Resolve::FilePath`] and
    /// [`Resolve::RealPath`] each link resolved, `tsrm_realpath_r`, every
    /// absolute path met entered in and read from the realpath cache;
    /// `None` where php answers `1`, an empty path, one of `MAXPATHLEN`
    /// bytes or more, and under `RealPath` one that is not there.
    ///
    /// ```php
    /// $dir = dirname(realpath(__FILE__));
    /// var_dump(realpath_cache_size() > 0, isset(realpath_cache_get()[$dir]), realpath_cache_get()[$dir]['is_dir'], realpath_cache_get()[$dir]['realpath'] === realpath($dir));
    /// clearstatcache(true, $dir); var_dump(isset(realpath_cache_get()[$dir]));
    /// clearstatcache(true); var_dump(realpath_cache_size(), realpath_cache_get());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// int(0)
    /// array(0) {
    /// }
    /// ```
    pub fn virtual_file_ex(self, cwd: &[u8], path: &[u8], how: Resolve) -> Option<Vec<u8>> {
        crate::virtual_cwd::virtual_file_ex(&self.0.shared.realpath_cache, cwd, path, how)
    }

    /// `clearstatcache(true, ...)`'s half: `realpath_cache_del` of one
    /// path, or with none `realpath_cache_clean`.
    pub fn realpath_cache_clear(self, path: Option<&[u8]>) {
        match path {
            Some(path) => self.0.shared.realpath_cache.delete(path),
            None => self.0.shared.realpath_cache.clean(),
        }
    }
}

/// The length of a path's scheme where it has one as
/// `php_stream_locate_url_wrapper` reads it: alphanumerics, `+`, `-` and
/// `.`, two or more, then `://`, or `data:` alone.
#[must_use]
pub fn scheme_length(path: &[u8]) -> Option<usize> {
    let n = path
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        .count();
    (n > 1
        && path.get(n) == Some(&b':')
        && (path[n + 1..].starts_with(b"//") || (n == 4 && path.starts_with(b"data:"))))
    .then_some(n)
}

/// Whether a path names a stream wrapper other than the plain files one,
/// which an include opens through that wrapper.
pub(crate) fn is_wrapper_url(path: &[u8]) -> bool {
    scheme_length(path).is_some_and(|n| !path[..n].eq_ignore_ascii_case(b"file"))
}
