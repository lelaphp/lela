//! The wrapper operations beside the opener (design §4.3 seam 5): php's
//! `php_stream_wrapper_ops` past `stream_opener` and `dir_opener`, which
//! are `url_stat`, `unlink`, `rename`, `stream_mkdir`, `stream_rmdir` and
//! `stream_metadata`, for the plain files wrapper
//! (`Z/main/streams/plain_wrapper.c:1272-1649`) and for one a program
//! registered, each reached through `php_stream_locate_url_wrapper` as the
//! opener is, and with the checks the natives of `file.c` and
//! `filestat.c` make before they reach one; `php://` has none of them.
//!
//! Beside them is `filestat.c`'s stat cache, `BG(CurrentStatFile)` and
//! `BG(CurrentLStatFile)` with the buffers they name, which lives here
//! and not in `ext/standard` because php's plain files kind clears it
//! (`php_clear_stat_cache` after every `read(2)`, `write(2)` and flush of
//! a plain file, and after its open), and `php_open_temporary_fd_ex`
//! (`Z/main/php_open_temporary_file.c:318`), which `tempnam`, `tmpfile`
//! and the spill of `php://temp` all make their file with.
//!
//! The request's `umask` is its own, so that one request's `umask()` is
//! not another's; a file or directory this layer creates is given the
//! permission bits `mode & ~umask` whatever the process's mask left, which
//! is what php's `umask(2)` then `open(2)` or `mkdir(2)` leave it.

use std::fs::{DirBuilder, File, FileTimes, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::api::raising::{Level, Thrown, docref, warning};
use crate::api::request::{Request, UrlStat, WrapperOp};
use crate::api::values::{Key, PhpString};
use crate::streams::{Located, StatBuf, stat_buf, strerror};

/// `S_IFMT` and `S_IFLNK`: the file type bits of `st_mode`, and a
/// symbolic link's.
const S_IFMT: i64 = 0o170_000;
const S_IFLNK: i64 = 0o120_000;

/// The thirteen fields of a stat buffer by name, in `php_fstat`'s
/// order (`Z/ext/standard/file.c:1327`).
const STAT_NAMES: [&[u8]; 13] = [
    b"dev", b"ino", b"mode", b"nlink", b"uid", b"gid", b"rdev", b"size", b"atime", b"mtime",
    b"ctime", b"blksize", b"blocks",
];

/// `EXDEV`, the `errno` of a `rename(2)` across devices, which is `18`
/// on every platform php builds for.
const EXDEV: i32 = 18;

/// `EPERM`, the one `errno` the cross-device rename's `chown(2)` and
/// `chmod(2)` may meet and still count as done.
const EPERM: i32 = 1;

/// `BG(CurrentStatFile)` and `BG(ssb)`, `BG(CurrentLStatFile)` and
/// `BG(lssb)` (`Z/ext/standard/basic_functions.h:79`): the path `php_stat`
/// last stated, as the program wrote it, and what it answered, one for
/// `stat(2)` and one for `lstat(2)`.
#[derive(Default)]
pub(crate) struct StatCache {
    stat: Option<(Vec<u8>, StatBuf)>,
    lstat: Option<(Vec<u8>, StatBuf)>,
}

/// `"file://"`, stripped once more by each plain files operation
/// (`strncasecmp(url, "file://", sizeof("file://") - 1)`), since the
/// natives hand it the path as written.
fn strip_file_scheme(url: &[u8]) -> &[u8] {
    if url.len() >= 7 && url[..7].eq_ignore_ascii_case(b"file://") {
        &url[7..]
    } else {
        url
    }
}

/// The path as the display of a diagnostic has it.
fn shown(path: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(path)
}

impl Request {
    /// The label `wrapper->wops->label` gives a wrapper, which the
    /// natives' refusals print.
    fn wrapper_label(&self, wrapper: &Located) -> &'static str {
        match wrapper {
            Located::Plain => "plainfile",
            Located::Php => "PHP",
            Located::Data => "RFC2397",
            Located::Ext(at) => self.wrappers.get(*at).map_or("user-space", |w| w.label),
            Located::Space(_) | Located::Nothing => "user-space",
        }
    }
}

/// Whether two located wrappers are one, `wrapper !=
/// php_stream_locate_url_wrapper(new_name, NULL, 0)` in `rename`.
fn same(a: &Located, b: &Located) -> bool {
    match (a, b) {
        (Located::Plain, Located::Plain)
        | (Located::Php, Located::Php)
        | (Located::Data, Located::Data) => true,
        (Located::Ext(x), Located::Ext(y)) => x == y,
        (Located::Space(x), Located::Space(y)) => x.same(y),
        _ => false,
    }
}

/// A `utimbuf`'s seconds as a time `utime(2)` sets.
fn at_seconds(seconds: i64) -> SystemTime {
    let magnitude = Duration::from_secs(seconds.unsigned_abs());
    if seconds >= 0 {
        SystemTime::UNIX_EPOCH + magnitude
    } else {
        SystemTime::UNIX_EPOCH - magnitude
    }
}

/// A `zend_long` as the `uid_t`, `gid_t` or `mode_t` php casts it to:
/// the low bits, two's complement, so `-1` is every bit set.
fn low_bits(value: i64, mask: i64) -> u32 {
    u32::try_from(value & mask).unwrap_or(u32::MAX)
}

/// `expand_filepath_with_mode(path, buf, NULL, 0, CWD_EXPAND)`
/// (`Z/main/fopen_wrappers.c:787`): the path made absolute against the
/// working directory and normalised as `virtual_file_ex` normalises it,
/// runs of slashes as one, `.` dropped and `..` taking the component
/// before it, with no trailing slash and no symbolic link read.
fn expand_filepath(cwd: &Path, path: &[u8]) -> Vec<u8> {
    let mut whole = Vec::new();
    if path.first() != Some(&b'/') {
        whole.extend_from_slice(cwd.as_os_str().as_bytes());
        whole.push(b'/');
    }
    whole.extend_from_slice(path);
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in whole.split(|b| *b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let mut out = Vec::new();
    for part in &parts {
        out.push(b'/');
        out.extend_from_slice(part);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

/// The bytes before the first NUL, a C string's.
fn c_string(buf: &[u8]) -> &[u8] {
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    &buf[..end]
}

/// A path of bytes as the host's.
fn host_bytes(path: &[u8]) -> PathBuf {
    PathBuf::from(std::ffi::OsStr::from_bytes(path))
}

impl Request {
    /// `php_clear_stat_cache(0, NULL, 0)` (`Z/ext/standard/filestat.c:697`):
    /// both entries of the stat cache dropped.
    pub(crate) fn forget_stats(&mut self) {
        self.stat_cache = StatCache::default();
    }

    /// `php_clear_stat_cache(1, NULL, 0)`, what the plain files wrapper's
    /// `unlink`, `rename` and `rmdir` do after they change the tree
    /// (`Z/main/streams/plain_wrapper.c:1321`, `:1428`, `:1560`): the
    /// stat cache and the realpath cache emptied.
    pub(crate) fn forget_stats_and_realpaths(&mut self) {
        self.forget_stats();
        self.realpath_cache_clear(None);
    }

    /// What `chdir` does to the stat cache (`Z/ext/standard/dir.c:367`):
    /// an entry whose path is relative is dropped, since it now names
    /// another file.
    pub(crate) fn forget_relative_stats(&mut self) {
        let relative = |entry: &Option<(Vec<u8>, StatBuf)>| {
            entry
                .as_ref()
                .is_some_and(|(path, _)| path.first() != Some(&b'/'))
        };
        if relative(&self.stat_cache.stat) {
            self.stat_cache.stat = None;
        }
        if relative(&self.stat_cache.lstat) {
            self.stat_cache.lstat = None;
        }
    }

    /// A path as the plain files wrapper's calls reach it: against the
    /// working directory, and the empty path left empty, which every
    /// call answers `ENOENT` to.
    fn plain_host(&self, url: &[u8]) -> PathBuf {
        if url.is_empty() {
            PathBuf::new()
        } else {
            self.absolutize(url).0
        }
    }

    /// `php_error_docref1(NULL, param, E_WARNING, ...)`: the native, the
    /// path in parentheses, and the message.
    fn docref1(&mut self, param: &[u8], message: &str) {
        let text = format!("{}({}): {message}", self.active_native(), shown(param));
        warning(self, &text);
    }

    /// `_php_stream_stat_path` (`Z/main/streams/streams.c:2168`), and
    /// with `cached` the stat `php_stat` makes over it
    /// (`Z/ext/standard/filestat.c:806-863`): with `cached` the cache
    /// is read first, by the path as written, `BG(CurrentLStatFile)`
    /// under `link` and `BG(CurrentStatFile)` otherwise; the wrapper is
    /// located and its `url_stat` asked, `lstat(2)` under `link` and
    /// `stat(2)` otherwise for the plain files wrapper; and with
    /// `cached` an answer is dropped into the cache, the link's entry
    /// under `link` and the other one too unless the answer is a
    /// symbolic link's. `None` is the stat that failed or a wrapper with
    /// no `url_stat`, `php://` among them.
    pub(crate) fn stat_url(
        &mut self,
        path: &[u8],
        link: bool,
        cached: bool,
        quiet: bool,
    ) -> Result<Option<StatBuf>, Thrown> {
        if cached {
            let entry = if link {
                &self.stat_cache.lstat
            } else {
                &self.stat_cache.stat
            };
            if let Some((name, sb)) = entry
                && name == path
            {
                return Ok(Some(*sb));
            }
        }
        let (wrapper, local) = self.locate_wrapper(path);
        let sb = match self.wrapper_stat(wrapper, &local, link, cached, quiet)? {
            UrlStat::Found(sb) => Some(sb),
            UrlStat::Failed | UrlStat::Unsupported => None,
        };
        if cached && let Some(sb) = sb {
            if link {
                self.stat_cache.lstat = Some((path.to_vec(), sb));
            }
            if !link || sb[2] & S_IFMT != S_IFLNK {
                self.stat_cache.stat = Some((path.to_vec(), sb));
            }
        }
        Ok(sb)
    }

    /// `php_stream_locate_url_wrapper(path, &local, 0)` and the located
    /// wrapper's own `url_stat` under `PHP_STREAM_URL_STAT_QUIET`, as
    /// `php_libxml_streams_IO_open_wrapper` asks it before it opens
    /// (`Z/ext/libxml/libxml.c:487`): no warning, no stat cache, and a
    /// wrapper without a `url_stat` told apart from one whose stat failed.
    pub(crate) fn stat_url_quiet(&mut self, path: &[u8]) -> Result<UrlStat, Thrown> {
        let quiet = crate::streams::OpenOptions {
            report: false,
            ..crate::streams::OpenOptions::REPORT
        };
        let (wrapper, local) = self.locate_for(path, quiet);
        self.wrapper_stat(wrapper, &local, false, false, true)
    }

    /// `wrapper->wops->url_stat(wrapper, local, flags, &ssb, NULL)` over a
    /// located wrapper: `lstat(2)` under `link` and `stat(2)` otherwise
    /// for the plain files wrapper, a program's `url_stat` with `link`,
    /// `quiet` and `cached` as its flags, an extension's own stat, and
    /// none for `php://` and for no wrapper.
    fn wrapper_stat(
        &mut self,
        wrapper: Located,
        local: &[u8],
        link: bool,
        cached: bool,
        quiet: bool,
    ) -> Result<UrlStat, Thrown> {
        let found = |sb: Option<StatBuf>| sb.map_or(UrlStat::Failed, UrlStat::Found);
        Ok(match wrapper {
            Located::Plain => {
                // `php_plain_files_url_stater` strips `file://` again
                // unless `PHP_STREAM_URL_STAT_IGNORE_OPEN_BASEDIR` is
                // set, which `php_stat` sets and `_php_stream_stat_path`
                // does not.
                let url = if cached {
                    local
                } else {
                    strip_file_scheme(local)
                };
                if url.is_empty() {
                    UrlStat::Failed
                } else {
                    let target = self.absolutize(url).0;
                    let meta = if link {
                        std::fs::symlink_metadata(target)
                    } else {
                        std::fs::metadata(target)
                    };
                    found(meta.ok().map(|meta| stat_buf(&meta)))
                }
            }
            Located::Space(user) => {
                // `php_stat` adds `PHP_STREAM_URL_STAT_IGNORE_OPEN_BASEDIR`,
                // and `PHP_STREAM_URL_STAT_QUIET` for an existence check.
                let flags = i64::from(link) | i64::from(quiet) << 1 | i64::from(cached) << 2;
                found(crate::streams::userspace::url_stat(
                    self, &user, local, flags,
                )?)
            }
            Located::Ext(at) => match self.wrappers[at].stat {
                Some(stat) => match stat(self, local)? {
                    Some(array) => {
                        // `statbuf_from_array`
                        // (`Z/main/streams/userspace.c:799`): each of the
                        // thirteen read by its name, zero where it is not
                        // there.
                        let mut sb = [0; 13];
                        for (field, name) in sb.iter_mut().zip(STAT_NAMES) {
                            let key = Key::Str(PhpString::new(self, name)?);
                            *field = array
                                .get(&key)
                                .and_then(|value| value.deref().as_int())
                                .unwrap_or(0);
                        }
                        UrlStat::Found(sb)
                    }
                    None => UrlStat::Failed,
                },
                None => UrlStat::Unsupported,
            },
            Located::Php | Located::Data | Located::Nothing => UrlStat::Unsupported,
        })
    }

    /// `php_stream_locate_url_wrapper(path, &local, 0) ==
    /// &php_plain_files_wrapper`: the path the plain files wrapper would
    /// be handed, or `None` where the path is another wrapper's or no
    /// wrapper's, after the warnings the locate itself raises.
    pub(crate) fn plain_local(&mut self, path: &[u8]) -> Option<Vec<u8>> {
        match self.locate_wrapper(path) {
            (Located::Plain, local) => Some(local),
            _ => None,
        }
    }

    /// One wrapper operation at a path, as the native that asks for it
    /// reaches the wrapper (`Z/ext/standard/file.c`, `filestat.c`), with
    /// php's warnings where the path has no wrapper or its wrapper lacks
    /// the operation.
    pub(crate) fn path_op(&mut self, path: &[u8], op: WrapperOp<'_>) -> Result<bool, Thrown> {
        match op {
            WrapperOp::Unlink => self.unlink_url(path),
            WrapperOp::Rename { to } => self.rename_url(path, to),
            WrapperOp::Mkdir { mode, recursive } => {
                // `_php_stream_mkdir` (`Z/main/streams/streams.c:2140`):
                // no wrapper, or one without `stream_mkdir`, is `0` and
                // nothing said.
                match self.locate_wrapper(path).0 {
                    Located::Plain => Ok(self.plain_mkdir(path, mode, recursive)),
                    Located::Ext(at) => match self.wrappers[at].mkdir {
                        Some(mkdir) => mkdir(self, path, mode, recursive),
                        None => Ok(false),
                    },
                    Located::Space(user) => {
                        crate::streams::userspace::path_op(self, &user, path, op)
                    }
                    Located::Php | Located::Data | Located::Nothing => Ok(false),
                }
            }
            WrapperOp::Rmdir => {
                // `_php_stream_rmdir` (`Z/main/streams/streams.c:2154`):
                // as `_php_stream_mkdir`.
                match self.locate_wrapper(path).0 {
                    Located::Plain => Ok(self.plain_rmdir(path)),
                    Located::Ext(at) => match self.wrappers[at].rmdir {
                        Some(rmdir) => rmdir(self, path),
                        None => Ok(false),
                    },
                    Located::Space(user) => {
                        crate::streams::userspace::path_op(self, &user, path, op)
                    }
                    Located::Php | Located::Data | Located::Nothing => Ok(false),
                }
            }
            WrapperOp::Touch(_)
            | WrapperOp::Chmod(_)
            | WrapperOp::Owner(_)
            | WrapperOp::Group(_)
            | WrapperOp::OwnerName(_)
            | WrapperOp::GroupName(_) => self.metadata_url(path, op),
        }
    }

    /// `PHP_FUNCTION(unlink)` (`Z/ext/standard/file.c:1235`) past its
    /// parameters: no wrapper is php's `Unable to locate stream wrapper`,
    /// one without `unlink` its `%s does not allow unlinking`, and the
    /// wrapper handed the path as written.
    fn unlink_url(&mut self, path: &[u8]) -> Result<bool, Thrown> {
        let wrapper = self.locate_wrapper(path).0;
        let unlink = match &wrapper {
            Located::Nothing => {
                docref(self, Level::Warning, "Unable to locate stream wrapper");
                return Ok(false);
            }
            Located::Plain => return Ok(self.plain_unlink(path)),
            Located::Space(user) => {
                return crate::streams::userspace::path_op(self, user, path, WrapperOp::Unlink);
            }
            Located::Ext(at) => self.wrappers[*at].unlink,
            Located::Php | Located::Data => None,
        };
        if let Some(unlink) = unlink {
            return unlink(self, path);
        }
        let text = format!("{} does not allow unlinking", self.wrapper_label(&wrapper));
        docref(self, Level::Warning, &text);
        Ok(false)
    }

    /// `php_plain_files_unlink` (`Z/main/streams/plain_wrapper.c:1298`):
    /// `unlink(2)`, php's warning with the path and `strerror(errno)`
    /// where it failed, and the stat cache cleared where it did not.
    fn plain_unlink(&mut self, url: &[u8]) -> bool {
        let url = strip_file_scheme(url);
        match std::fs::remove_file(self.plain_host(url)) {
            Ok(()) => {
                self.forget_stats_and_realpaths();
                true
            }
            Err(error) => {
                self.docref1(url, &strerror(&error));
                false
            }
        }
    }

    /// `PHP_FUNCTION(rename)` (`Z/ext/standard/file.c:1196`) past its
    /// parameters: the source's wrapper located, no wrapper being
    /// `Unable to locate stream wrapper` and one without `rename`
    /// `%s wrapper does not support renaming` under its label, `glob` or
    /// `ZLIB` for an extension's; then the target's, which
    /// must be the same wrapper, `Cannot rename a file across wrapper
    /// types` otherwise; then the wrapper's `rename` over the two paths
    /// as written.
    ///
    /// ```php
    /// var_dump(rename('glob:///tmp/x', 'glob:///tmp/y'), rename('compress.zlib:///tmp/x', 'compress.zlib:///tmp/y'));
    /// ```
    /// ```expect
    /// Warning: rename(): glob wrapper does not support renaming in %s on line 2
    ///
    /// Warning: rename(): ZLIB wrapper does not support renaming in %s on line 2
    /// bool(false)
    /// bool(false)
    /// ```
    fn rename_url(&mut self, from: &[u8], to: &[u8]) -> Result<bool, Thrown> {
        let wrapper = self.locate_wrapper(from).0;
        let rename = match &wrapper {
            Located::Nothing => {
                docref(self, Level::Warning, "Unable to locate stream wrapper");
                return Ok(false);
            }
            Located::Plain | Located::Space(_) => None,
            Located::Ext(at) if self.wrappers[*at].rename.is_some() => self.wrappers[*at].rename,
            Located::Ext(_) | Located::Php | Located::Data => {
                let text = format!(
                    "{} wrapper does not support renaming",
                    self.wrapper_label(&wrapper)
                );
                docref(self, Level::Warning, &text);
                return Ok(false);
            }
        };
        if !same(&wrapper, &self.locate_wrapper(to).0) {
            docref(
                self,
                Level::Warning,
                "Cannot rename a file across wrapper types",
            );
            return Ok(false);
        }
        if let Located::Space(user) = &wrapper {
            return crate::streams::userspace::path_op(self, user, from, WrapperOp::Rename { to });
        }
        match rename {
            Some(rename) => rename(self, from, to),
            None => Ok(self.plain_rename(from, to)),
        }
    }

    /// `php_plain_files_rename` (`Z/main/streams/plain_wrapper.c:1326`):
    /// `rename(2)`; across devices, `EXDEV`, the file copied, given the
    /// source's owner and permission bits where it may be, and the
    /// source unlinked;
    /// a failure is php's warning with both paths, and a rename done
    /// clears the stat cache, which the copy across devices does not.
    fn plain_rename(&mut self, from: &[u8], to: &[u8]) -> bool {
        let from = strip_file_scheme(from);
        let to = strip_file_scheme(to);
        let (source, target) = (self.plain_host(from), self.plain_host(to));
        let docref2 = |req: &mut Request, error: &std::io::Error| {
            let text = format!(
                "{}({},{}): {}",
                req.active_native(),
                shown(from),
                shown(to),
                strerror(error)
            );
            warning(req, &text);
        };
        match std::fs::rename(&source, &target) {
            Ok(()) => {
                self.forget_stats_and_realpaths();
                true
            }
            Err(error) if error.raw_os_error() == Some(EXDEV) => {
                use std::os::unix::fs::MetadataExt;
                if let Err(error) = std::fs::copy(&source, &target) {
                    docref2(self, &error);
                    return false;
                }
                let meta = match std::fs::metadata(&source) {
                    Ok(meta) => meta,
                    Err(error) => {
                        docref2(self, &error);
                        return false;
                    }
                };
                let mut success = true;
                if let Err(error) =
                    std::os::unix::fs::chown(&target, Some(meta.uid()), Some(meta.gid()))
                {
                    docref2(self, &error);
                    success = error.raw_os_error() == Some(EPERM);
                }
                if success
                    && let Err(error) = std::fs::set_permissions(
                        &target,
                        std::fs::Permissions::from_mode(meta.mode() & 0o7777),
                    )
                {
                    docref2(self, &error);
                    success = error.raw_os_error() == Some(EPERM);
                }
                if success {
                    let _ = std::fs::remove_file(&source);
                }
                success
            }
            Err(error) => {
                docref2(self, &error);
                false
            }
        }
    }

    /// The permission bits a file or directory this layer created is
    /// given, `mode & ~umask` as `open(2)` and `mkdir(2)` apply the mask
    /// php's `umask()` set, whatever the process's own mask took off.
    pub(crate) fn apply_umask(&self, path: &Path, mode: u32) {
        let want = mode & 0o777 & !self.umask();
        if let Ok(meta) = std::fs::metadata(path) {
            let have = meta.permissions().mode();
            if have & 0o777 != want {
                let _ = std::fs::set_permissions(
                    path,
                    std::fs::Permissions::from_mode((have & !0o777 & 0o7777) | want),
                );
            }
        }
    }

    /// `mkdir(2)` of one directory with `mode`, the request's mask
    /// applied where it made one.
    fn make_dir(&self, path: &Path, mode: u32) -> std::io::Result<()> {
        DirBuilder::new().mode(mode).create(path)?;
        self.apply_umask(path, mode);
        Ok(())
    }

    /// `php_plain_files_mkdir` (`Z/main/streams/plain_wrapper.c:1433`):
    /// one `mkdir(2)` and php's warning, `strerror(errno)`, where it
    /// failed; with `$recursive` the path expanded, the deepest
    /// directory of it that exists found by stating each parent from the
    /// end, and each component after it made in turn, one that exists
    /// already passed over and the last one's failure the warning.
    fn plain_mkdir(&mut self, dir: &[u8], mode: i64, recursive: bool) -> bool {
        let dir = strip_file_scheme(dir);
        let mode = low_bits(mode, 0o7777);
        if !recursive {
            return match self.make_dir(&self.plain_host(dir), mode) {
                Ok(()) => true,
                Err(error) => {
                    docref(self, Level::Warning, &strerror(&error));
                    false
                }
            };
        }
        let mut buf = expand_filepath(&self.cwd().0, dir);
        let e = buf.len();
        let dir_len = dir.len();
        let mut p = buf[..dir_len.min(e)].iter().position(|b| *b == b'/');
        let offset = p.map_or(0, |at| at + 1);
        if !(p.is_some() && dir_len == 1) {
            loop {
                let len = c_string(&buf).len();
                let found = buf
                    .get(offset..len)
                    .and_then(|s| s.iter().rposition(|b| *b == b'/'))
                    .map(|at| at + offset)
                    .or_else(|| {
                        (offset != 1)
                            .then(|| buf[..len].iter().rposition(|b| *b == b'/'))
                            .flatten()
                    });
                p = found;
                let Some(mut at) = found else {
                    break;
                };
                let mut n = 0;
                buf[at] = 0;
                while at > 0 && buf[at - 1] == b'/' {
                    n += 1;
                    at -= 1;
                    buf[at] = 0;
                }
                if std::fs::metadata(host_bytes(c_string(&buf))).is_ok() {
                    loop {
                        buf[at] = b'/';
                        if n == 0 {
                            break;
                        }
                        n -= 1;
                        at += 1;
                    }
                    p = Some(at);
                    break;
                }
            }
        }
        let mut p = p.unwrap_or(0);
        loop {
            let created = self.make_dir(&host_bytes(c_string(&buf)), mode);
            if let Err(error) = &created
                && error.kind() != std::io::ErrorKind::AlreadyExists
            {
                docref(self, Level::Warning, &strerror(error));
                return false;
            }
            let mut replaced_slash = false;
            loop {
                p += 1;
                if p >= e {
                    break;
                }
                if buf[p] == 0 {
                    replaced_slash = true;
                    buf[p] = b'/';
                    if buf.get(p + 1).copied().unwrap_or(0) != 0 {
                        break;
                    }
                }
            }
            if p >= e || !replaced_slash {
                if let Err(error) = created {
                    docref(self, Level::Warning, &strerror(&error));
                    return false;
                }
                return true;
            }
        }
    }

    /// `php_plain_files_rmdir` (`Z/main/streams/plain_wrapper.c:1536`):
    /// `rmdir(2)`, php's warning with the path and `strerror(errno)`
    /// where it failed, and the stat cache cleared where it did not.
    fn plain_rmdir(&mut self, url: &[u8]) -> bool {
        let url = strip_file_scheme(url);
        match std::fs::remove_dir(self.plain_host(url)) {
            Ok(()) => {
                self.forget_stats_and_realpaths();
                true
            }
            Err(error) => {
                self.docref1(url, &strerror(&error));
                false
            }
        }
    }

    /// The wrapper half of `touch`, `chmod`, `chown` and `chgrp`
    /// (`Z/ext/standard/filestat.c:318-690`), the half a path takes when
    /// its wrapper is not the plain files one or it is written with
    /// `file://`: the wrapper's `stream_metadata` where it has one;
    /// otherwise php's `Cannot call %s() for a non-standard stream`,
    /// except that a `touch` with no times opens the path `c` through
    /// its wrapper and closes it.
    fn metadata_url(&mut self, path: &[u8], op: WrapperOp<'_>) -> Result<bool, Thrown> {
        let wrapper = self.locate_wrapper(path).0;
        match wrapper {
            Located::Plain => return Ok(self.plain_metadata(path, op)),
            Located::Ext(at) => {
                if let Some(set_stat) = self.wrappers[at].set_stat {
                    return set_stat(self, path, op);
                }
            }
            Located::Space(user) => {
                return crate::streams::userspace::path_op(self, &user, path, op);
            }
            Located::Php | Located::Data | Located::Nothing => {}
        }
        let name = match op {
            WrapperOp::Touch(None) => {
                let Some(stream) = self.open_stream_wrapper(path, b"c", false)? else {
                    return Ok(false);
                };
                stream.free(self);
                return Ok(true);
            }
            WrapperOp::Touch(Some(_)) => "touch",
            WrapperOp::Chmod(_) => "chmod",
            WrapperOp::Owner(_) | WrapperOp::OwnerName(_) => "chown",
            _ => "chgrp",
        };
        let text = format!("Cannot call {name}() for a non-standard stream");
        docref(self, Level::Warning, &text);
        Ok(false)
    }

    /// `php_plain_files_metadata` (`Z/main/streams/plain_wrapper.c:1565`):
    /// for `touch` the file made first where `access(F_OK)` finds none,
    /// php's `Unable to create file %s because %s` where it cannot be,
    /// then `utime(2)`; `chown(2)` of the owner or the group; `chmod(2)`;
    /// a call that failed is php's `Operation failed: %s`, and one done
    /// clears the stat cache.
    fn plain_metadata(&mut self, url: &[u8], op: WrapperOp<'_>) -> bool {
        let url = strip_file_scheme(url);
        let host = self.plain_host(url);
        let done = match op {
            WrapperOp::Touch(times) => {
                if std::fs::metadata(&host).is_err() {
                    match OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .mode(0o666)
                        .open(&host)
                    {
                        Ok(_) => self.apply_umask(&host, 0o666),
                        Err(error) => {
                            let text = format!(
                                "Unable to create file {} because {}",
                                shown(url),
                                strerror(&error)
                            );
                            self.docref1(url, &text);
                            return false;
                        }
                    }
                }
                set_times(&host, times)
            }
            WrapperOp::Owner(uid) => {
                std::os::unix::fs::chown(&host, Some(low_bits(uid, 0xFFFF_FFFF)), None)
            }
            WrapperOp::Group(gid) => {
                std::os::unix::fs::chown(&host, None, Some(low_bits(gid, 0xFFFF_FFFF)))
            }
            WrapperOp::Chmod(mode) => std::fs::set_permissions(
                &host,
                std::fs::Permissions::from_mode(low_bits(mode, 0o7777)),
            ),
            WrapperOp::Unlink
            | WrapperOp::Rename { .. }
            | WrapperOp::Mkdir { .. }
            | WrapperOp::Rmdir
            | WrapperOp::OwnerName(_)
            | WrapperOp::GroupName(_) => return false,
        };
        match done {
            Ok(()) => {
                self.forget_stats();
                true
            }
            Err(error) => {
                let text = format!("Operation failed: {}", strerror(&error));
                self.docref1(url, &text);
                false
            }
        }
    }

    /// `php_open_temporary_fd_ex` (`Z/main/php_open_temporary_file.c:318`)
    /// under `PHP_TMP_FILE_DEFAULT`: the file made in `dir`, or where
    /// that fails php's notice `file created in the system's temporary
    /// directory` and the file made there; no `dir`, or an empty one,
    /// goes to the temporary directory with nothing said. The file is
    /// open for reading and writing, and `None` is the one that could
    /// not be made anywhere.
    pub(crate) fn open_temporary_fd(
        &mut self,
        dir: Option<&[u8]>,
        prefix: &[u8],
    ) -> Option<(File, PathBuf)> {
        if let Some(dir) = dir.filter(|dir| !dir.is_empty()) {
            let dir = self.absolutize(dir).0;
            if let Some(made) = open_temporary_in(&dir, prefix) {
                return Some(made);
            }
            docref(
                self,
                Level::Notice,
                "file created in the system's temporary directory",
            );
        }
        open_temporary_in(&self.temp_dir().0, prefix)
    }
}

/// `php_do_open_temporary_file` (`Z/main/php_open_temporary_file.c:92`):
/// the directory resolved as `realpath(3)` resolves it, the prefix
/// followed by thirteen characters of `base32alphabet` from a random
/// word, and `mkstemp(3)`'s six, a new six tried where the name is
/// taken; the file `0600`, open for reading and writing.
fn open_temporary_in(dir: &Path, prefix: &[u8]) -> Option<(File, PathBuf)> {
    use std::hash::{BuildHasher, Hasher};
    const BASE32: &[u8] = b"0123456789abcdefghijklmnopqrstuv";
    const MKSTEMP: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let random = |salt: u64| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(salt);
        hasher.finish()
    };
    let real = std::fs::canonicalize(dir).ok()?;
    let mut name = prefix.to_vec();
    let mut word = random(0);
    for _ in 0..13 {
        name.push(BASE32[usize::try_from(word % 32).unwrap_or(0)]);
        word /= 32;
    }
    for attempt in 1u64..=100 {
        let mut candidate = name.clone();
        let mut word = random(attempt);
        for _ in 0..6 {
            candidate.push(MKSTEMP[usize::try_from(word % 62).unwrap_or(0)]);
            word /= 62;
        }
        let path = real.join(std::ffi::OsStr::from_bytes(&candidate));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Some((file, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
    }
    None
}

/// `utime(2)` with a `utimbuf` of the two times, or the current time for
/// `None`, through the file opened for reading, or for writing where it
/// cannot be read.
fn set_times(path: &Path, times: Option<(i64, i64)>) -> std::io::Result<()> {
    let file = File::open(path).or_else(|_| OpenOptions::new().write(true).open(path))?;
    let now = SystemTime::now();
    let (modified, accessed) = times.map_or((now, now), |(mtime, atime)| {
        (at_seconds(mtime), at_seconds(atime))
    });
    file.set_times(
        FileTimes::new()
            .set_accessed(accessed)
            .set_modified(modified),
    )
}
