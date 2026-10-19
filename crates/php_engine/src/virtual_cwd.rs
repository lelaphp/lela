//! `Z/Zend/zend_virtual_cwd.c`'s path resolution, `virtual_file_ex` over
//! `tsrm_realpath_r`, which `expand_filepath` (`Z/main/fopen_wrappers.c:787`),
//! `VCWD_REALPATH` and the engine's own include and primary-file
//! resolution reach: a path made absolute against a working directory,
//! `.` and `..` taken out, and, where the file is there, each symbolic
//! link on the way replaced by what it points at; and the realpath cache
//! beside it, `CWDG(realpath_cache)`, which every absolute path resolved
//! under `CWD_FILEPATH` or `CWD_REALPATH` is entered in and read from,
//! and which `realpath_cache_get()` and `realpath_cache_size()` show. The
//! separator is `/` alone and [`MAXPATHLEN`] is the host's.
//!
//! The cache is the engine's (#2941): php's is the process's in the
//! non-ZTS build php-fpm runs, so a child's second request finds what its
//! first entered and stats nothing until an entry's `realpath_cache_ttl`
//! expires, and here one engine is what a child is, serving every thread
//! (design §5.14). Each lookup and each entry takes the cache's mutex for
//! itself alone, so no system call is made under it; an entry another
//! request made between a lookup and its entry stands, and the second is
//! not made, which php's one thread never meets.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

/// `MAXPATHLEN` (`Z/Zend/zend_virtual_cwd.h:39`), the host's
/// `PATH_MAX`: 4096 on Linux, 1024 on macOS and the BSDs. The one value
/// the path checks, `open_basedir`, `include` and `dl()` read and
/// `PHP_MAXPATHLEN` says, and the one an extension reads as
/// `request::MAXPATHLEN`, as php's extensions read the one of
/// `zend_virtual_cwd.h`.
pub const MAXPATHLEN: usize = if cfg!(target_os = "linux") {
    4096
} else {
    1024
};

/// `LINK_MAX` as `zend_virtual_cwd.c` redefines it: how many links one
/// resolution follows before it gives up.
const LINK_MAX: i32 = 32;

/// `CWD_EXPAND`, `CWD_FILEPATH` and `CWD_REALPATH`: expand `.` and `..`
/// only; resolve links where the file is there and expand otherwise;
/// resolve links and fail where the file is not there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolve {
    Expand,
    FilePath,
    RealPath,
}

/// The slots of `CWDG(realpath_cache)`, a bucket's key modulo which
/// chooses its chain.
const SLOTS: usize = 1024;

/// `sizeof(realpath_cache_bucket)` on a 64-bit host without `ZEND_WIN32`:
/// five words, two `uint16_t` and a bit field, padded to eight.
const BUCKET_SIZE: i64 = 48;

/// One `realpath_cache_bucket`: a path as it was written, after `.` and
/// `..`, and what it resolved to.
#[derive(Debug)]
pub(crate) struct Bucket {
    pub(crate) key: u64,
    pub(crate) path: Vec<u8>,
    pub(crate) realpath: Vec<u8>,
    pub(crate) is_dir: bool,
    pub(crate) expires: i64,
}

impl Bucket {
    /// What `realpath_cache_add` counts the bucket as: the structure,
    /// the path with its NUL, and the resolved path with its NUL where
    /// it is not the same bytes.
    pub(crate) fn size(&self) -> i64 {
        let mut size = BUCKET_SIZE + len_of(self.path.len()) + 1;
        if self.realpath != self.path {
            size += len_of(self.realpath.len()) + 1;
        }
        size
    }
}

fn len_of(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `CWDG(realpath_cache)` and the two values php keeps beside it in
/// `CWDG`, the engine's: the buckets every request's resolutions enter
/// and read, `realpath_cache_size_limit`, which an `open_basedir` sets to
/// zero, the cache off for the rest of the process, and
/// `realpath_cache_ttl`.
#[derive(Debug)]
pub(crate) struct RealpathCache {
    buckets: Mutex<Buckets>,
    limit: AtomicI64,
    ttl: i64,
    /// The paths the resolutions through this cache asked the file
    /// system about, `lstat`, for the tests to hold a request to none.
    #[cfg(test)]
    looked_up: std::sync::atomic::AtomicUsize,
}

/// The slots and `CWDG(realpath_cache_size)`, the bytes their buckets
/// count. A bucket is shared, so a lookup that finds one copies a pointer
/// under the mutex and reads the bucket after it.
#[derive(Debug)]
struct Buckets {
    slots: Vec<Vec<Arc<Bucket>>>,
    size: i64,
}

impl RealpathCache {
    /// The entries php keeps in `CWDG`, `PHP_INI_SYSTEM` both, from what
    /// `value` gives their master values, as `OnUpdateLong` writes them
    /// (`Z/main/main.c:880`); an `open_basedir` among them turns the cache
    /// off, as `php_module_startup` does (`Z/main/main.c:2312`,
    /// [`RealpathCache::engine_startup`]).
    pub(crate) fn of<'a>(value: impl Fn(&[u8]) -> Option<&'a [u8]>) -> RealpathCache {
        let mut cache = RealpathCache {
            buckets: Mutex::new(Buckets {
                slots: vec![Vec::new(); SLOTS],
                size: 0,
            }),
            limit: AtomicI64::new(0),
            ttl: 0,
            #[cfg(test)]
            looked_up: std::sync::atomic::AtomicUsize::new(0),
        };
        for name in [&b"realpath_cache_size"[..], b"realpath_cache_ttl"] {
            cache.update(name, value(name));
        }
        cache.engine_startup(value(b"open_basedir"));
        cache
    }

    /// The master value of `name` set to `value`, where `name` is one the
    /// cache keeps, the limit or the time to live, as `OnUpdateLong` writes
    /// it. An `open_basedir` set here turns nothing off: php checks it at
    /// the engine's startup and at each request's
    /// ([`RealpathCache::engine_startup`], [`RealpathCache::turn_off`]),
    /// so a php-fpm pool's `php_admin_value[open_basedir]`, set on the
    /// master value after the engine started, leaves the cache on until
    /// the child's first request starts, and that request's
    /// `init_request_info` enters its program's path.
    pub(crate) fn update(&mut self, name: &[u8], value: Option<&[u8]>) {
        let parsed = || value.map_or(0, crate::api::request::Request::ini_parse_bytes);
        match name {
            b"realpath_cache_size" => *self.limit.get_mut() = parsed(),
            b"realpath_cache_ttl" => self.ttl = parsed(),
            _ => {}
        }
    }

    /// `php_module_startup`'s `Disable realpath cache if an open_basedir is
    /// set` (`Z/main/main.c:2312`), over the master `open_basedir` the
    /// engine started with.
    pub(crate) fn engine_startup(&self, open_basedir: Option<&[u8]>) {
        if open_basedir.is_some_and(|list| !list.is_empty()) {
            self.turn_off();
        }
    }

    fn buckets(&self) -> std::sync::MutexGuard<'_, Buckets> {
        self.buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How many paths the resolutions through this cache asked the file
    /// system about since it was made.
    #[cfg(test)]
    pub(crate) fn looked_up(&self) -> usize {
        self.looked_up.load(Ordering::Relaxed)
    }

    /// `CWDG(realpath_cache_size_limit)`, zero while the cache is off.
    fn limit(&self) -> i64 {
        self.limit.load(Ordering::Relaxed)
    }

    /// `CWDG(realpath_cache_size_limit) = 0`, as an `open_basedir` a
    /// request starts under sets it (`php_request_startup`,
    /// `Z/main/main.c:1918`): off for every request after it, as php's
    /// process global stays.
    pub(crate) fn turn_off(&self) {
        self.limit.store(0, Ordering::Relaxed);
    }

    /// `realpath_cache_key`: FNV-1 over the path's bytes, each read as
    /// a C `char`, signed on the hosts php builds for here.
    fn key(path: &[u8]) -> u64 {
        path.iter().fold(2_166_136_261_u64, |h, &c| {
            let c = i64::from(i8::from_ne_bytes([c]));
            h.wrapping_mul(16_777_619) ^ u64::from_ne_bytes(c.to_ne_bytes())
        })
    }

    fn slot(key: u64) -> usize {
        usize::try_from(key % SLOTS as u64).unwrap_or(0)
    }

    /// `realpath_cache_find`: the bucket for `path`, the expired buckets
    /// met on its chain dropped on the way.
    fn find(&self, path: &[u8], now: i64) -> Option<Arc<Bucket>> {
        let key = RealpathCache::key(path);
        let mut buckets = self.buckets();
        let Buckets { slots, size } = &mut *buckets;
        let chain = &mut slots[RealpathCache::slot(key)];
        let mut at = 0;
        while at < chain.len() {
            if self.ttl != 0 && chain[at].expires < now {
                *size -= chain.remove(at).size();
                continue;
            }
            if chain[at].key == key && chain[at].path == path {
                return Some(Arc::clone(&chain[at]));
            }
            at += 1;
        }
        None
    }

    /// `realpath_cache_add`, where the limit leaves room and no other
    /// request entered `path` since this one looked.
    fn add(&self, path: &[u8], realpath: &[u8], is_dir: bool, now: i64) {
        let bucket = Arc::new(Bucket {
            key: RealpathCache::key(path),
            path: path.to_vec(),
            realpath: realpath.to_vec(),
            is_dir,
            expires: now.saturating_add(self.ttl),
        });
        let bytes = bucket.size();
        let mut buckets = self.buckets();
        let Buckets { slots, size } = &mut *buckets;
        let chain = &mut slots[RealpathCache::slot(bucket.key)];
        let entered = chain
            .iter()
            .any(|had| had.key == bucket.key && had.path == bucket.path);
        if !entered && *size + bytes <= self.limit() {
            chain.insert(0, bucket);
            *size += bytes;
        }
    }

    /// `realpath_cache_del`: the bucket for `path` dropped.
    pub(crate) fn delete(&self, path: &[u8]) {
        let key = RealpathCache::key(path);
        let mut buckets = self.buckets();
        let Buckets { slots, size } = &mut *buckets;
        let chain = &mut slots[RealpathCache::slot(key)];
        if let Some(at) = chain.iter().position(|b| b.key == key && b.path == path) {
            *size -= chain.remove(at).size();
        }
    }

    /// `realpath_cache_clean`: every bucket dropped.
    pub(crate) fn clean(&self) {
        let mut buckets = self.buckets();
        for chain in &mut buckets.slots {
            chain.clear();
        }
        buckets.size = 0;
    }

    /// Each bucket, as `shown`, in `realpath_cache_get()`'s order: slot by
    /// slot, each chain newest first.
    pub(crate) fn listed<T>(&self, mut shown: impl FnMut(&Bucket) -> T) -> Vec<T> {
        self.buckets()
            .slots
            .iter()
            .flatten()
            .map(|bucket| shown(bucket))
            .collect()
    }
}

/// `*t` of `tsrm_realpath_r`: the time, read once on the first lookup,
/// or `-1` where the cache has no time to live, which php never reads the
/// time for and enters as each bucket's `expires`
/// (`Z/Zend/zend_virtual_cwd.c:1140`).
struct Now {
    at: Option<i64>,
}

impl Now {
    fn new(ttl: i64) -> Now {
        Now {
            at: (ttl == 0).then_some(-1),
        }
    }

    fn get(&mut self) -> i64 {
        *self.at.get_or_insert_with(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        })
    }
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
    walk: &mut Walk<'_>,
    p: &mut Buf,
    start: usize,
    mut len: usize,
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
            return match realpath_r(walk, p, start, i - 1, how, true, None) {
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
        let mut save = how != Resolve::Expand;
        if start > 0 && save && walk.cache.limit() != 0 {
            // The cache lookup for an absolute path.
            let now = walk.now.get();
            if let Some(bucket) = walk.cache.find(&p.head(len), now) {
                if is_dir && !bucket.is_dir {
                    return None;
                }
                if let Some(d) = link_is_dir.as_deref_mut() {
                    *d = bucket.is_dir;
                }
                p.write(0, &bucket.realpath);
                return Some(bucket.realpath.len());
            }
        }
        let tmp = p.head(len);
        let meta = if save {
            #[cfg(test)]
            walk.cache.looked_up.fetch_add(1, Ordering::Relaxed);
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
            walk.links += 1;
            if walk.links > LINK_MAX {
                return None;
            }
            let target = std::fs::read_link(host(&tmp)).ok()?;
            let target = target.as_os_str().as_bytes();
            let target = target.get(..MAXPATHLEN).unwrap_or(target).to_vec();
            let j = if target.first() == Some(&b'/') {
                p.write(0, &target);
                realpath_r(walk, p, 1, target.len(), how, is_dir, Some(&mut directory))?
            } else {
                if i + target.len() >= MAXPATHLEN - 1 {
                    return None;
                }
                p.write(i, &target);
                p.write(0, tmp.get(..i - 1).unwrap_or_default());
                p.put(i - 1, b'/');
                realpath_r(
                    walk,
                    p,
                    start,
                    i + target.len(),
                    how,
                    is_dir,
                    Some(&mut directory),
                )?
            };
            if let Some(d) = link_is_dir.as_deref_mut() {
                *d = directory;
            }
            walk.save(start, &tmp, &p.head(j), directory);
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
            let j = realpath_r(walk, p, start, i - 1, prefix, true, None)?;
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
        let j = j + (len - i);
        if save {
            walk.save(start, &tmp, &p.head(j), directory);
        }
        return Some(j);
    }
}

/// What one `virtual_file_ex` carries down its recursion: the links
/// followed, `*ll`, the time, `*t`, and the cache it reads and fills.
struct Walk<'c> {
    links: i32,
    now: Now,
    cache: &'c RealpathCache,
}

impl Walk<'_> {
    /// `realpath_cache_add(tmp, len, path, j, directory, *t)` for an
    /// absolute path, where the cache is on.
    fn save(&mut self, start: usize, path: &[u8], realpath: &[u8], is_dir: bool) {
        if start > 0 && self.cache.limit() != 0 {
            let now = self.now.get();
            self.cache.add(path, realpath, is_dir, now);
        }
    }
}

/// `virtual_file_ex(state, path, NULL, how)` (`Z/Zend/zend_virtual_cwd.c:1007`)
/// with `state` the directory `cwd`: the path resolved, or `None` where
/// php answers `1`, an empty path or one of `MAXPATHLEN` or more among
/// them.
pub(crate) fn virtual_file_ex(
    cache: &RealpathCache,
    cwd: &[u8],
    path: &[u8],
    how: Resolve,
) -> Option<Vec<u8>> {
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
    let mut walk = Walk {
        links: 0,
        now: Now::new(cache.ttl),
        cache,
    };
    let mut len = realpath_r(&mut walk, &mut buf, start, length, how, false, None)?;
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

#[cfg(test)]
mod tests {
    /// Linux's `PATH_MAX` is 4096, which php's `MAXPATHLEN` and so
    /// `PHP_MAXPATHLEN` are there; the macOS oracle, at 1024, cannot
    /// show it, so no witness holds it.
    #[cfg(target_os = "linux")]
    #[test]
    fn maxpathlen_is_linux_path_max() {
        assert_eq!(super::MAXPATHLEN, 4096);
    }
}
