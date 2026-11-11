//! The `open_basedir` guard (design §4.4, revision 42): php's
//! `php_check_open_basedir_ex` with `php_check_specific_open_basedir`
//! under it and `OnUpdateBaseDir` beside it (`Z/main/fopen_wrappers.c:74-337`),
//! resolved through the virtual cwd ([`crate::virtual_cwd`]) as php's
//! resolve through `expand_filepath` and `VCWD_REALPATH`. It is the only
//! constructor of [`HostPath`], so a path the host's filesystem is handed
//! by name has been through it; it runs where php runs it, in the plain
//! files and glob wrappers and in the natives that reach no wrapper, with
//! php's one bit of whether a refusal warns.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::api::raising::{Level, docref};
use crate::api::request::Environment;
use crate::api::request::Request;
use crate::virtual_cwd::{MAXPATHLEN, RealpathCache, Resolve, virtual_file_ex};

/// `DEFAULT_DIR_SEPARATOR`, what separates the base directories.
const SEPARATOR: u8 = b':';

/// A path on the host's filesystem, absolute, after the request's `cwd`
/// and `open_basedir` have been applied: the path as the program wrote
/// it, made absolute against the working directory, which
/// [`Environment::check_open_basedir`] answers for a path it lets through and
/// nothing else makes. The empty path stays empty, which every call on
/// the host answers `ENOENT` to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostPath(PathBuf);

impl std::ops::Deref for HostPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for HostPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl From<HostPath> for PathBuf {
    fn from(path: HostPath) -> PathBuf {
        path.0
    }
}

/// Why the guard refused a path, the `errno` `php_check_open_basedir_ex`
/// leaves for the caller to answer its own failure with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// `EPERM`: the path is within none of the base directories.
    Outside,
    /// `EINVAL`: the path is `MAXPATHLEN` bytes or longer.
    TooLong,
}

impl From<Refused> for std::io::Error {
    fn from(refused: Refused) -> std::io::Error {
        const EPERM: i32 = 1;
        const EINVAL: i32 = 22;
        std::io::Error::from_raw_os_error(match refused {
            Refused::Outside => EPERM,
            Refused::TooLong => EINVAL,
        })
    }
}

fn host(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

impl Request {
    /// The path under `STREAM_DISABLE_OPEN_BASEDIR`, what `mail.log` is
    /// opened by: the one open php makes past the guard, let through here
    /// so that it stands beside it.
    pub(crate) fn unguarded_host_path(&self, path: &[u8]) -> HostPath {
        self.host_path(path)
    }

    /// The path made absolute against the working directory, the empty
    /// path left empty.
    fn host_path(&self, path: &[u8]) -> HostPath {
        let path = if path.is_empty() {
            PathBuf::new()
        } else {
            self.environment().absolutize(path)
        };
        HostPath(path)
    }

    /// The working directory the guard resolves against: the request's
    /// virtual one.
    fn guard_cwd(&self) -> Vec<u8> {
        self.environment()
            .cwd()
            .into_os_string()
            .into_encoded_bytes()
    }

    /// [`within`] in the request's working directory.
    fn within(&mut self, basedir: &[u8], path: &[u8]) -> bool {
        let cwd = self.guard_cwd();
        within(&self.shared.realpath_cache, &cwd, basedir, path)
    }
}

/// `php_check_specific_open_basedir(basedir, path)`
/// (`Z/main/fopen_wrappers.c:142`): the path expanded and resolved to
/// its deepest part that exists, a broken link at its end read once,
/// and held to the base directory expanded, `.` being the working
/// directory `cwd`, with a `/` after it, so `/a` holds `/a/b` and not
/// `/ab`; each resolution `virtual_file_ex`'s through `cache` against
/// `cwd`, as `expand_filepath` and `VCWD_REALPATH` resolve.
fn within(cache: &RealpathCache, cwd: &[u8], basedir: &[u8], path: &[u8]) -> bool {
    let expand = |path: &[u8]| virtual_file_ex(cache, cwd, path, Resolve::FilePath);
    let local = if basedir == b"." {
        cwd.to_vec()
    } else {
        basedir.to_vec()
    };
    if path.len() > MAXPATHLEN - 1 {
        return false;
    }
    let Some(mut resolved_name) = expand(path) else {
        return false;
    };
    let mut path_len = resolved_name.len();
    let mut path_tmp = resolved_name.clone();
    let mut nesting = 0;
    loop {
        if let Some(real) = virtual_file_ex(cache, cwd, &path_tmp, Resolve::RealPath) {
            resolved_name = real;
            break;
        }
        if nesting == 0
            && let Ok(target) = std::fs::read_link(host(&path_tmp))
        {
            // "put the real path into the path buffer"
            path_tmp = target.into_os_string().into_encoded_bytes();
            path_tmp.truncate(MAXPATHLEN - 1);
        }
        // "none of the path components exist. definitely not in
        // open_basedir.."
        let Some(slash) = path_tmp.iter().rposition(|b| *b == b'/') else {
            return false;
        };
        path_len = slash + 1;
        path_tmp.truncate(slash);
        if path_tmp.is_empty() {
            // "Do not pass an empty string to realpath(), as this
            // will resolve to CWD."
            break;
        }
        nesting += 1;
    }
    let Some(mut resolved_basedir) = expand(&local) else {
        return false;
    };
    if basedir.last() == Some(&b'/') {
        if resolved_basedir.last() != Some(&b'/') {
            resolved_basedir.push(b'/');
        }
    } else {
        resolved_basedir.push(b'/');
    }
    // `path_tmp[path_len - 1]`: the expanded path's own last byte where
    // it resolved at once, and the NUL a shortened one was cut at.
    if path_tmp.get(path_len - 1) == Some(&b'/') && resolved_name.last() != Some(&b'/') {
        resolved_name.push(b'/');
    }
    if resolved_name.starts_with(&resolved_basedir) {
        return true;
    }
    // "/openbasedir/ and /openbasedir are the same directory"
    resolved_basedir.len() == resolved_name.len() + 1
        && resolved_basedir.starts_with(&resolved_name)
}

/// What `php_check_open_basedir_ex` decides of `path` under the list
/// `list` in the working directory `cwd`, before it warns of anything: a
/// path of `MAXPATHLEN` bytes or more is [`Refused::TooLong`], one within
/// none of the base directories [`Refused::Outside`].
fn verdict(cache: &RealpathCache, cwd: &[u8], list: &[u8], path: &[u8]) -> Result<(), Refused> {
    if path.len() > MAXPATHLEN - 1 {
        return Err(Refused::TooLong);
    }
    if base_dirs(list).any(|dir| within(cache, cwd, dir, path)) {
        Ok(())
    } else {
        Err(Refused::Outside)
    }
}

impl crate::Engine {
    /// `php_check_open_basedir_ex(path, 0)` outside any request, under
    /// the `open_basedir` every request starts with and against the
    /// process's working directory: the [`HostPath`] of a file a host
    /// serves itself, past every request, as makala sends a static file,
    /// which no request is standing to warn in. An empty list guards
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`Refused`], the path within none of the base directories or too
    /// long.
    pub fn check_open_basedir(&self, path: &[u8]) -> Result<HostPath, Refused> {
        let cwd = std::env::current_dir()
            .map(|dir| dir.into_os_string().into_encoded_bytes())
            .unwrap_or_default();
        let list = self
            .ini_value(b"open_basedir")
            .filter(|list| !list.is_empty());
        if let Some(list) = list {
            verdict(&self.shared.realpath_cache, &cwd, &list, path)?;
        }
        let given = host(path);
        Ok(HostPath(if path.is_empty() || given.is_absolute() {
            given.to_path_buf()
        } else {
            host(&cwd).join(given)
        }))
    }
}

impl Request {
    /// `OnUpdateBaseDir` at runtime (`Z/main/fopen_wrappers.c:88-131`): a
    /// new list is taken only where it is no wider than the one in force,
    /// each of its directories expanded and let through by the guard
    /// without a word; the expanded list is what `PG(open_basedir)` keeps
    /// ([`Environment::open_basedir`]), and the entry the value as set. `None`
    /// refuses it, the empty list among them, which `ini_set()` answers
    /// `false` to. At startup any value is taken as it is written.
    pub(crate) fn tighter_open_basedir(&mut self, value: &[u8]) -> Option<Vec<u8>> {
        if value.is_empty() {
            return None;
        }
        let mut kept = Vec::new();
        let mut rest = value;
        while !rest.is_empty() {
            let (dir, after) = match rest.iter().position(|b| *b == SEPARATOR) {
                Some(at) => (&rest[..at], &rest[at + 1..]),
                None => (rest, &rest[rest.len()..]),
            };
            let resolved = self.environment_mut().expand_filepath(dir)?;
            self.environment_mut()
                .check_open_basedir(&resolved, false)
                .ok()?;
            if !kept.is_empty() {
                kept.push(SEPARATOR);
            }
            kept.extend_from_slice(&resolved);
            rest = after;
        }
        Some(kept)
    }
}

impl Environment<&mut Request> {
    /// `php_check_open_basedir_ex(path, warn)`
    /// (`Z/main/fopen_wrappers.c:291`): with `open_basedir` set, a path of
    /// `MAXPATHLEN` bytes or more is php's warning that it is too long and
    /// [`Refused::TooLong`], and one within none of the base directories
    /// is, under `warn`, php's `open_basedir restriction in effect`
    /// warning, and [`Refused::Outside`]; a refusal never throws, and the
    /// caller turns it into its own failure. Any other path is let
    /// through as the [`HostPath`] the host is handed.
    ///
    /// ```php
    /// ini_set('open_basedir', __DIR__);
    /// var_dump(fopen('/', 'r'), glob('/*'), is_file(__FILE__));
    /// var_dump(ini_set('open_basedir', '/'), ini_get('open_basedir') === __DIR__);
    /// ```
    /// ```expect
    /// Warning: fopen(): open_basedir restriction in effect. File(/) is not within the allowed path(s): (%s) in %s on line 3
    ///
    /// Warning: fopen(/): Failed to open stream: Operation not permitted in %s on line 3
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// ```
    pub fn check_open_basedir(self, path: &[u8], warn: bool) -> Result<HostPath, Refused> {
        // `PG(open_basedir) && *PG(open_basedir)`: an empty list guards
        // nothing.
        let list = self
            .0
            .environment()
            .open_basedir()
            .filter(|list| !list.is_empty())
            .map(<[u8]>::to_vec);
        if let Some(list) = list {
            let cwd = self.0.guard_cwd();
            match verdict(&self.0.shared.realpath_cache, &cwd, &list, path) {
                Ok(()) => {}
                Err(Refused::TooLong) => {
                    let text = format!(
                        "File name is longer than the maximum allowed path length on this platform ({MAXPATHLEN}): {}",
                        String::from_utf8_lossy(path)
                    );
                    docref(self.0, Level::Warning, &text);
                    return Err(Refused::TooLong);
                }
                Err(Refused::Outside) => {
                    if warn {
                        let text = format!(
                            "open_basedir restriction in effect. File({}) is not within the allowed path(s): ({})",
                            String::from_utf8_lossy(path),
                            String::from_utf8_lossy(&list)
                        );
                        docref(self.0, Level::Warning, &text);
                    }
                    return Err(Refused::Outside);
                }
            }
        }
        Ok(self.0.host_path(path))
    }

    /// `expand_filepath(path, buf)`: the path against the working
    /// directory under `CWD_FILEPATH`, links resolved where the file is
    /// there; `None` for the empty path and one too long.
    pub fn expand_filepath(self, path: &[u8]) -> Option<Vec<u8>> {
        let cwd = self
            .0
            .environment()
            .cwd()
            .into_os_string()
            .into_encoded_bytes();
        self.0
            .environment_mut()
            .virtual_file_ex(&cwd, path, Resolve::FilePath)
    }

    /// `php_check_specific_open_basedir(basedir, path) == 0`: whether
    /// `path`, expanded and resolved to its deepest part that exists,
    /// lies under `basedir` (`.` the working directory), which warns of
    /// nothing. What mysqlnd's `LOAD DATA LOCAL INFILE` holds a file to
    /// its local infile directory with (`mysqlnd_loaddata.c`).
    pub fn within_basedir(self, basedir: &[u8], path: &[u8]) -> bool {
        self.0.within(basedir, path)
    }
}

/// The directories of the list, `while (ptr && *ptr)` over its
/// `DEFAULT_DIR_SEPARATOR`: an empty one between two separators is one
/// that holds nothing, and a separator at the end closes the list.
fn base_dirs(list: &[u8]) -> impl Iterator<Item = &[u8]> {
    let list = list.strip_suffix(&[SEPARATOR]).unwrap_or(list);
    list.split(|b| *b == SEPARATOR)
}
