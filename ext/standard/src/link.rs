//! `Z/ext/standard/link.c`: `readlink`, `linkinfo`, `symlink` and
//! `link`, each a system call over a path the program wrote, with
//! `strerror(errno)` as the warning where it failed. Each asks the
//! `open_basedir` guard first: `readlink` of the link, `linkinfo` of the
//! directory it stands in, and the two that make one of the target and
//! then of the link, each refusal a warning and `false`. The two that make
//! a link expand their paths first with `expand_filepath`
//! (`crate::virtual_cwd`), which resolves a link that is already there,
//! so `symlink` onto an existing link makes the link at what it points
//! to, as php's does. php's `Unable to symlink to a URL` refusal reads the
//! expanded path, which on this host always begins with `/` and so never
//! names a wrapper; it is left out.
//!
//! The four, as the stub declares them.
//!
//! ```php
//! echo new ReflectionFunction('readlink'), new ReflectionFunction('linkinfo'), new ReflectionFunction('symlink'), new ReflectionFunction('link');
//! ```
//! ```expect
//! Function [ <internal:standard> function readlink ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> string $path ]
//!   }
//!   - Return [ string|false ]
//! }
//! Function [ <internal:standard> function linkinfo ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> string $path ]
//!   }
//!   - Return [ int|false ]
//! }
//! Function [ <internal:standard> function symlink ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> string $target ]
//!     Parameter #1 [ <required> string $link ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function link ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> string $target ]
//!     Parameter #1 [ <required> string $link ]
//!   }
//!   - Return [ bool ]
//! }
//! ```
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;

use php_ext::abi::{OrFalse, Path};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{MAXPATHLEN, Request};
use php_ext::values::{Owner, PhpString};

use crate::filestat::strerror;
use crate::string::path::zend_dirname;
use crate::virtual_cwd::expand_filepath;

fn host(bytes: &[u8]) -> &std::path::Path {
    std::path::Path::new(OsStr::from_bytes(bytes))
}

/// The path a system call is handed for what the program wrote: against
/// the request's working directory, the empty path left empty so the
/// call answers `ENOENT`.
fn plain(req: &Request, path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return Vec::new();
    }
    req.environment()
        .absolutize(path)
        .as_os_str()
        .as_bytes()
        .to_vec()
}

/// The request's working directory as bytes, what `VCWD_GETCWD` gives
/// `expand_filepath`.
fn cwd(req: &Request) -> Vec<u8> {
    req.environment().cwd().as_os_str().as_bytes().to_vec()
}

/// `php_error_docref(NULL, E_WARNING, "%s", strerror(errno))`, and the
/// `false` after it.
fn failed(req: &mut Request, error: &std::io::Error) -> bool {
    docref(req, Level::Warning, &strerror(error));
    false
}

/// `readlink(string $path): string|false`, `PHP_FUNCTION(readlink)`
/// (`Z/ext/standard/link.c:45`): `readlink(2)` of the path, at most
/// `MAXPATHLEN - 1` bytes of what the link holds, and `strerror(errno)`
/// where it failed, which for a file that is no link is `EINVAL`'s.
///
/// ```php
/// $d = __DIR__ . '/readlink-d'; @mkdir($d); chdir($d);
/// symlink('there', 'l'); file_put_contents('f', 'x');
/// var_dump(readlink('l'), readlink("$d/l"), readlink('f'), readlink('nope'), readlink(''));
/// unlink('l'); unlink('f'); chdir('/'); rmdir($d);
/// try { readlink("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: readlink(): Invalid argument in %s on line 4
///
/// Warning: readlink(): No such file or directory in %s on line 4
///
/// Warning: readlink(): No such file or directory in %s on line 4
/// string(5) "there"
/// string(5) "there"
/// bool(false)
/// bool(false)
/// bool(false)
/// readlink(): Argument #1 ($path) must not contain any null bytes
/// ```
#[php_function]
fn readlink(req: &mut Request, path: Path) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Ok(path) = req
        .environment_mut()
        .check_open_basedir(path.0.as_bytes(), true)
    else {
        return Ok(OrFalse::False);
    };
    match std::fs::read_link(&path) {
        Ok(target) => {
            let target = target.as_os_str().as_bytes();
            let target = target.get(..MAXPATHLEN - 1).unwrap_or(target);
            Ok(OrFalse::Value(PhpString::new(req, target)?))
        }
        Err(error) => {
            failed(req, &error);
            Ok(OrFalse::False)
        }
    }
}

/// `linkinfo(string $path): int|false`, `PHP_FUNCTION(linkinfo)`
/// (`Z/ext/standard/link.c:80`): `lstat(2)`'s `st_dev` for the path,
/// and `-1` after `strerror(errno)` where it failed; php answers `-1`
/// and never `false`.
///
/// ```php
/// $d = __DIR__ . '/linkinfo-d'; @mkdir($d); chdir($d);
/// symlink('nowhere', 'l');
/// var_dump(linkinfo('l') === lstat('l')['dev'], linkinfo('l') > 0, linkinfo('nope'), linkinfo(''));
/// unlink('l'); chdir('/'); rmdir($d);
/// ```
/// ```expect
/// Warning: linkinfo(): No such file or directory in %s on line 4
///
/// Warning: linkinfo(): No such file or directory in %s on line 4
/// bool(true)
/// bool(true)
/// int(-1)
/// int(-1)
/// ```
#[php_function]
fn linkinfo(req: &mut Request, path: Path) -> OrFalse<i64> {
    // The guard is asked of the directory the link stands in.
    let dirname = zend_dirname(path.0.as_bytes());
    if req
        .environment_mut()
        .check_open_basedir(&dirname, true)
        .is_err()
    {
        return OrFalse::False;
    }
    let path = plain(req, path.0.as_bytes());
    match std::fs::symlink_metadata(host(&path)) {
        Ok(meta) => OrFalse::Value(i64::try_from(meta.dev()).unwrap_or(i64::MAX)),
        Err(error) => {
            docref(req, Level::Warning, &strerror(&error));
            OrFalse::Value(-1)
        }
    }
}

/// `symlink(string $target, string $link): bool`,
/// `PHP_FUNCTION(symlink)` (`Z/ext/standard/link.c:111`): the link's
/// path expanded against the working directory and the target's against
/// the link's directory, either failing as `No such file or directory`,
/// then `symlink(2)` with the target exactly as written, since a
/// relative target is relative to the link and not to the working
/// directory, and `strerror(errno)` where it failed. The link's path is
/// expanded with its links resolved, so a second `symlink` onto the same
/// name makes the link the first one points at.
///
/// ```php
/// $d = __DIR__ . '/symlink-d'; @mkdir($d); chdir($d);
/// var_dump(symlink('t.txt', 'l'), readlink('l'), is_link('l'), symlink('t.txt', 'sub/../m'), readlink('m'));
/// var_dump(symlink('x', 'nodir/l'), symlink('', 'l2'), symlink('x', ''), symlink('t.txt', 'l'), is_link('t.txt'), readlink('t.txt'));
/// foreach (['l', 'm', 't.txt'] as $f) unlink($f); chdir('/'); rmdir($d);
/// ```
/// ```expect
/// bool(true)
/// string(5) "t.txt"
/// bool(true)
/// bool(true)
/// string(5) "t.txt"
///
/// Warning: symlink(): No such file or directory in %s on line 4
///
/// Warning: symlink(): No such file or directory in %s on line 4
///
/// Warning: symlink(): No such file or directory in %s on line 4
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// string(5) "t.txt"
/// ```
#[php_function]
fn symlink(req: &mut Request, target: Path, link: Path) -> bool {
    let (Path(target), Path(link)) = (target, link);
    let cwd_now = cwd(req);
    let Some(source) = expand_filepath(req, link.as_bytes(), &cwd_now) else {
        return no_such_file(req);
    };
    let dirname = zend_dirname(&source);
    let Some(dest) = expand_filepath(req, target.as_bytes(), &dirname) else {
        return no_such_file(req);
    };
    if req
        .environment_mut()
        .check_open_basedir(&dest, true)
        .is_err()
    {
        return false;
    }
    let Ok(source) = req.environment_mut().check_open_basedir(&source, true) else {
        return false;
    };
    // The target is written into the link as given, relative to the
    // link and not to the working directory.
    match std::os::unix::fs::symlink(host(target.as_bytes()), &source) {
        Ok(()) => true,
        Err(error) => failed(req, &error),
    }
}

/// `link(string $target, string $link): bool`, `PHP_FUNCTION(link)`
/// (`Z/ext/standard/link.c:171`): both paths expanded against the
/// working directory, either failing as `No such file or directory`,
/// then `link(2)` over the two expanded paths, as php's ZTS build calls
/// it, and `strerror(errno)` where it failed.
///
/// ```php
/// $d = __DIR__ . '/link-d'; @mkdir($d); chdir($d);
/// file_put_contents('t', 'x');
/// var_dump(link('t', 'h'), file_get_contents('h'), stat('t')['nlink'], link('t', 'h'), link('nope', 'h2'), link('', 'h3'), link('t', ''));
/// foreach (['t', 'h'] as $f) unlink($f); chdir('/'); rmdir($d);
/// ```
/// ```expect
/// Warning: link(): File exists in %s on line 4
///
/// Warning: link(): No such file or directory in %s on line 4
///
/// Warning: link(): No such file or directory in %s on line 4
///
/// Warning: link(): No such file or directory in %s on line 4
/// bool(true)
/// string(1) "x"
/// int(2)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn link(req: &mut Request, target: Path, link: Path) -> bool {
    let (Path(target), Path(link)) = (target, link);
    let cwd = cwd(req);
    let (Some(source), Some(dest)) = (
        expand_filepath(req, link.as_bytes(), &cwd),
        expand_filepath(req, target.as_bytes(), &cwd),
    ) else {
        return no_such_file(req);
    };
    let Ok(dest) = req.environment_mut().check_open_basedir(&dest, true) else {
        return false;
    };
    let Ok(source) = req.environment_mut().check_open_basedir(&source, true) else {
        return false;
    };
    match std::fs::hard_link(&dest, &source) {
        Ok(()) => true,
        Err(error) => failed(req, &error),
    }
}

/// The warning php writes itself where a path does not expand, and the
/// `false` after it.
fn no_such_file(req: &mut Request) -> bool {
    docref(req, Level::Warning, "No such file or directory");
    false
}
