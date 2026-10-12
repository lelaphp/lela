//! `Z/ext/standard/dir.c` past the four natives `file` declares over a
//! directory stream (`opendir`, `closedir`, `rewinddir`, `readdir`):
//! `dir` and the `Directory` class it hands back, `chdir`, `getcwd` and
//! `scandir` and `glob`, the last over php's bundled `php_glob.c`
//! (`glob`), with the constants `dir.stub.php` declares. The working
//! directory is the request's, as php's ZTS build keeps it per thread,
//! and `chdir` answers as the host's `chdir(2)` would, which is what the
//! CLI's `VCWD_CHDIR` is. `chroot` is not declared: the oracle's CLI is
//! built without `ENABLE_CHROOT_FUNC`.
//!
//! The natives, as the stub declares them.
//!
//! ```php
//! echo new ReflectionFunction('dir'), new ReflectionFunction('chdir'), new ReflectionFunction('getcwd'), new ReflectionFunction('scandir'), new ReflectionFunction('glob');
//! ```
//! ```expect
//! Function [ <internal:standard> function dir ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> string $directory ]
//!     Parameter #1 [ <optional> $context = null ]
//!   }
//!   - Return [ Directory|false ]
//! }
//! Function [ <internal:standard> function chdir ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> string $directory ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function getcwd ] {
//!
//!   - Parameters [0] {
//!   }
//!   - Return [ string|false ]
//! }
//! Function [ <internal:standard> function scandir ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> string $directory ]
//!     Parameter #1 [ <optional> int $sorting_order = SCANDIR_SORT_ASCENDING ]
//!     Parameter #2 [ <optional> $context = null ]
//!   }
//!   - Return [ array|false ]
//! }
//! Function [ <internal:standard> function glob ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> string $pattern ]
//!     Parameter #1 [ <optional> int $flags = 0 ]
//!   }
//!   - Return [ array|false ]
//! }
//! ```
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use php_ext::abi::{Instance, OrFalse, Path as PathArg, Refusals, This};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::call::{lookup_class, new_object};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw_error};
use php_ext::request::{MAXPATHLEN, OwnedStream as _, Request, Resolve, Stream, Streams, Whence};
use php_ext::values::{Owner, PhpArray, PhpString, Resource, Value, ValueRef};

use crate::file::set_default_dir;
use crate::streamsfuncs::{NullContext, context_of};
use crate::string::locale::collate;
use crate::{State, ffi};

mod glob_wrapper;
pub(crate) mod php_glob;
pub(crate) use glob_wrapper::wrapper as glob_wrapper;

use constants::{SCANDIR_SORT_ASCENDING, SCANDIR_SORT_NONE};

/// The constants of `dir.stub.php`, in its order.
///
/// ```php
/// var_dump(DIRECTORY_SEPARATOR, PATH_SEPARATOR, GLOB_BRACE, GLOB_ERR, GLOB_MARK, GLOB_NOCHECK, GLOB_NOESCAPE, GLOB_NOSORT, GLOB_ONLYDIR, GLOB_AVAILABLE_FLAGS, SCANDIR_SORT_ASCENDING, SCANDIR_SORT_DESCENDING, SCANDIR_SORT_NONE);
/// ```
/// ```expect
/// string(1) "/"
/// string(1) ":"
/// int(128)
/// int(4)
/// int(8)
/// int(16)
/// int(4096)
/// int(32)
/// int(1073741824)
/// int(1073746108)
/// int(0)
/// int(1)
/// int(2)
/// ```
#[php_const]
pub(crate) mod constants {
    use super::php_glob;

    pub(crate) const DIRECTORY_SEPARATOR: &str = "/";
    pub(crate) const PATH_SEPARATOR: &str = ":";
    pub(crate) const GLOB_BRACE: i32 = php_glob::PHP_GLOB_BRACE;
    pub(crate) const GLOB_ERR: i32 = php_glob::PHP_GLOB_ERR;
    pub(crate) const GLOB_MARK: i32 = php_glob::PHP_GLOB_MARK;
    pub(crate) const GLOB_NOCHECK: i32 = php_glob::PHP_GLOB_NOCHECK;
    pub(crate) const GLOB_NOESCAPE: i32 = php_glob::PHP_GLOB_NOESCAPE;
    pub(crate) const GLOB_NOSORT: i32 = php_glob::PHP_GLOB_NOSORT;
    pub(crate) const GLOB_ONLYDIR: i32 = php_glob::PHP_GLOB_ONLYDIR;
    pub(crate) const GLOB_AVAILABLE_FLAGS: i32 = php_glob::PHP_GLOB_AVAILABLE_FLAGS;
    /// `PHP_SCANDIR_SORT_*`, `scandir`'s `$sorting_order`.
    pub(crate) const SCANDIR_SORT_ASCENDING: i64 = 0;
    pub(crate) const SCANDIR_SORT_DESCENDING: i64 = 1;
    pub(crate) const SCANDIR_SORT_NONE: i64 = 2;
}

/// `Directory` as `register_class_Directory` declares it: final, no
/// dynamic properties, not serializable, the two readonly properties `path` and `handle`
/// in that order, since the methods read the `handle` slot as
/// `Z_DIRECTORY_HANDLE_P` does, and the three methods.
///
/// ```php
/// echo new ReflectionClass('Directory');
/// ```
/// ```expect
/// Class [ <internal:standard> final class Directory ] {
///
///   - Constants [0] {
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [0] {
///   }
///
///   - Properties [2] {
///     Property [ public protected(set) readonly string $path ]
///     Property [ public protected(set) readonly mixed $handle ]
///   }
///
///   - Methods [3] {
///     Method [ <internal:standard> public method close ] {
///
///       - Parameters [0] {
///       }
///       - Return [ void ]
///     }
///
///     Method [ <internal:standard> public method rewind ] {
///
///       - Parameters [0] {
///       }
///       - Return [ void ]
///     }
///
///     Method [ <internal:standard> public method read ] {
///
///       - Parameters [0] {
///       }
///       - Return [ string|false ]
///     }
///   }
/// }
/// ```
///
/// The three methods over the `handle` slot: a read after a rewind is the
/// first entry again, and after `close` each is php's `TypeError`.
///
/// ```php
/// $d = dir(__DIR__);
/// $first = $d->read(); $d->rewind(); var_dump($d->read() === $first, $d->close());
/// try { $d->read(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $d->rewind(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// NULL
/// Directory::read(): cannot use Directory resource after it has been closed
/// Directory::rewind(): cannot use Directory resource after it has been closed
/// ```
///
/// ```php
/// try { serialize(dir(__DIR__)); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Serialization of 'Directory' is not allowed
/// ```
#[php_class(
    final,
    not_serializable,
    handlers = DIRECTORY,
    dynamic_properties = forbidden
)]
pub(crate) struct Directory;

#[php_class]
impl Directory {
    #[php(property, readonly)]
    const path: PhpString;
    #[php(property, readonly)]
    const handle: Value;

    /// `Directory::close(): void`, `PHP_METHOD(Directory, close)`
    /// (`Z/ext/standard/dir.c:227`): the stream closed, and forgotten as
    /// the last opened one where it was that.
    fn close(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let stream = this_stream(req, this, "close")?;
        let res = stream.resource();
        stream.close(req);
        let is_default = req
            .state::<State>()
            .default_dir
            .as_deref()
            .is_some_and(|default| default.id() == res.id());
        if is_default {
            set_default_dir(req, None);
        }
        Ok(())
    }

    /// `Directory::rewind(): void`, `PHP_METHOD(Directory, rewind)`
    /// (`Z/ext/standard/dir.c:260`): `php_stream_rewinddir`.
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let stream = this_stream(req, this, "rewind")?;
        stream.seek(req, 0, Whence::Set);
        Ok(())
    }

    /// `Directory::read(): string|false`, `PHP_METHOD(Directory, read)`
    /// (`Z/ext/standard/dir.c:294`): the next entry's name, `false` past
    /// the last.
    fn read(req: &mut Request, this: This<Self>) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let stream = this_stream(req, this, "read")?;
        Ok(match stream.read_dir(req)? {
            Some(name) => OrFalse::Value(PhpString::new(req, &name)?),
            None => OrFalse::False,
        })
    }
}

/// `dir_class_object_handlers` (`Z/ext/standard/dir.c:88-96`):
/// `std_object_handlers` with `get_constructor` refusing, `clone_obj`
/// php's null slot, and `compare` `zend_objects_not_comparable`.
const DIRECTORY: Refusals = Refusals::NONE
    .construction(Some("dir()"))
    .cloning()
    .comparison();

/// `dir(string $directory, $context = null): Directory|false`,
/// `_php_do_opendir` with `createobject` (`Z/ext/standard/dir.c:105`):
/// the directory opened as `opendir` opens it, made the last opened
/// one, and a `Directory` with `path` the string as given and `handle`
/// the stream, or `false` after the open's warning. The methods read the
/// stream back; once closed it is php's `TypeError`. The class refuses
/// `new` through its `get_constructor`, which the engine's `NEW` does not
/// consult yet.
///
/// ```php
/// $d = dir(__DIR__); var_dump($d instanceof Directory, $d->path === __DIR__, is_resource($d->handle));
/// $names = []; while (($n = $d->read()) !== false) { $names[] = $n; } sort($names); var_dump($names[0], $names[1]);
/// $d->rewind(); var_dump($d->read()); $d->close();
/// try { $d->read(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { clone $d; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump(dir(__DIR__ . '/nope'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// string(1) "."
/// string(2) ".."
/// string(1) "."
/// Directory::read(): cannot use Directory resource after it has been closed
/// Trying to clone an uncloneable object of class Directory
///
/// Warning: dir(%s/nope): Failed to open directory: No such file or directory in %s on line 7
/// bool(false)
/// ```
#[php_function]
fn dir(
    req: &mut Request,
    directory: PathArg,
    #[php(default = null)] context: Option<Resource>,
) -> Result<OrFalse<Owner<Instance<Directory>>>, Thrown> {
    let PathArg(directory) = directory;
    let context = context_of(req, context, NullContext::Default)?;
    let Some(stream) = req.with_stream_ctx(context, |req| req.open_dir(directory.as_bytes()))?
    else {
        return Ok(OrFalse::False);
    };
    set_default_dir(req, Some(*stream));
    let Some(class) = lookup_class(req, b"Directory")? else {
        stream.give_up(req);
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: dir() without its Directory class",
        ));
    };
    let object = match new_object(req, class) {
        Ok(object) => object,
        Err(thrown) => {
            stream.give_up(req);
            return Err(thrown);
        }
    };
    let path = Owner::take(req, Value::string(directory));
    if let Err(thrown) = object.set_slot(req, 0, path) {
        stream.give_up(req);
        object.give_up(req);
        return Err(thrown);
    }
    if let Err(thrown) = object.set_slot(req, 1, stream.into_value()) {
        object.give_up(req);
        return Err(thrown);
    }
    Ok(OrFalse::Value(object.into_instance()))
}

/// `php_dir_get_directory_stream_from_this` (`Z/ext/standard/dir.c:184`):
/// the stream the object's `handle` slot holds, php's `Error` where the
/// slot holds no resource or a stream that is no directory, and its
/// `TypeError` naming the method where the resource is closed.
fn this_stream(req: &mut Request, this: This<Directory>, method: &str) -> Result<Stream, Thrown> {
    let handle = this.object().slot(1);
    let handle = handle.map(|value| value.deref());
    let Some(handle) = handle.filter(|value| matches!(value.as_ref(), ValueRef::Resource(_)))
    else {
        return Err(throw_error(
            req,
            "Error",
            "Internal directory stream has been altered",
        ));
    };
    let Some(stream) = Stream::from_value(req, &handle) else {
        return Err(throw_error(
            req,
            "TypeError",
            &format!(
                "Directory::{method}(): cannot use Directory resource after it has been closed"
            ),
        ));
    };
    if !stream.is_dir(req) {
        return Err(throw_error(
            req,
            "Error",
            "Internal directory stream has been altered",
        ));
    }
    Ok(stream)
}

fn host(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

/// `chdir(string $directory): bool`, `PHP_FUNCTION(chdir)`
/// (`Z/ext/standard/dir.c:348`): the request's working directory moved
/// to the directory, resolved as `chdir(2)` resolves it, links and all,
/// and the stat cache's entries for relative paths forgotten; where the
/// host's `chdir(2)` would refuse, php's warning with `strerror(errno)`
/// and the number.
///
/// ```php
/// $d = __DIR__ . '/chdir-sub'; @mkdir($d); touch(__DIR__ . '/chdir-file');
/// var_dump(chdir($d), basename(getcwd()), chdir('..'), getcwd() === dirname(getcwd() . '/x'), chdir('chdir-sub'), basename(getcwd()));
/// var_dump(chdir('nope'), chdir(__DIR__ . '/chdir-file'), chdir(''));
/// try { chdir("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(9) "chdir-sub"
/// bool(true)
/// bool(true)
/// bool(true)
/// string(9) "chdir-sub"
///
/// Warning: chdir(): No such file or directory (errno 2) in %s on line 4
///
/// Warning: chdir(): Not a directory (errno 20) in %s on line 4
///
/// Warning: chdir(): No such file or directory (errno 2) in %s on line 4
/// bool(false)
/// bool(false)
/// bool(false)
/// chdir(): Argument #1 ($directory) must not contain any null bytes
/// ```
#[php_function]
fn chdir(req: &mut Request, directory: PathArg) -> Result<bool, Thrown> {
    let PathArg(directory) = directory;
    let Ok(guarded) = req.check_open_basedir(directory.as_bytes(), true) else {
        return Ok(false);
    };
    let cwd = req.cwd().as_os_str().as_bytes().to_vec();
    let refused = |code: i32| Err(std::io::Error::from_raw_os_error(code));
    let target = match req.virtual_file_ex(&cwd, directory.as_bytes(), Resolve::RealPath) {
        None if directory.as_bytes().is_empty() => refused(libc_errno::ENOENT),
        None => std::fs::metadata(req.absolutize(directory.as_bytes()))
            .and_then(|_| refused(libc_errno::ENOENT)),
        Some(path) => match std::fs::metadata(host(&path)) {
            Ok(meta) if !meta.is_dir() => refused(libc_errno::ENOTDIR),
            Ok(_) if !ffi::access(host(&path), ffi::X_OK) => refused(libc_errno::EACCES),
            Ok(_) => Ok(path),
            Err(error) => Err(error),
        },
    };
    match target {
        Ok(_) => {
            req.set_cwd(&guarded)?;
            req.forget_relative_stats();
            Ok(true)
        }
        Err(error) => {
            let text = crate::filestat::strerror(&error);
            let code = error.raw_os_error().unwrap_or(0);
            docref(req, Level::Warning, &format!("{text} (errno {code})"));
            Ok(false)
        }
    }
}

/// The three `errno` values `chdir` names itself.
mod libc_errno {
    pub(super) const ENOENT: i32 = 2;
    pub(super) const EACCES: i32 = 13;
    pub(super) const ENOTDIR: i32 = 20;
}

/// `getcwd(): string|false`, `PHP_FUNCTION(getcwd)`
/// (`Z/ext/standard/dir.c:376`): the request's working directory.
///
/// ```php
/// var_dump(getcwd()[0], is_dir(getcwd()), getcwd() === getcwd());
/// try { getcwd(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "/"
/// bool(true)
/// bool(true)
/// getcwd() expects exactly 0 arguments, 1 given
/// ```
#[php_function]
fn getcwd(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let cwd = req.cwd();
    Ok(OrFalse::Value(PhpString::new(
        req,
        cwd.as_os_str().as_bytes(),
    )?))
}

/// `scandir(string $directory, int $sorting_order = SCANDIR_SORT_ASCENDING, $context = null): array|false`,
/// `PHP_FUNCTION(scandir)` (`Z/ext/standard/dir.c:520`) over
/// `php_stream_scandir`: every entry the directory stream gives, `.`
/// and `..` among them, sorted by `strcoll` under the request's
/// `LC_COLLATE`, which the C locale makes byte order, ascending for `SCANDIR_SORT_ASCENDING`, left in the
/// stream's order for `SCANDIR_SORT_NONE` and descending for any other
/// number; where the directory does not open, the open's warning and
/// then php's own with `errno`.
///
/// ```php
/// $d = __DIR__ . '/scandir-sub'; @mkdir($d); foreach (['b', 'A', '_x'] as $f) touch("$d/$f");
/// var_dump(scandir($d), implode(' ', scandir($d, SCANDIR_SORT_DESCENDING)), scandir($d, 7) === scandir($d, 1), count(scandir($d, SCANDIR_SORT_NONE)));
/// var_dump(scandir("$d/nope"), scandir("$d/b"));
/// try { scandir(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(5) {
///   [0]=>
///   string(1) "."
///   [1]=>
///   string(2) ".."
///   [2]=>
///   string(1) "A"
///   [3]=>
///   string(2) "_x"
///   [4]=>
///   string(1) "b"
/// }
/// string(11) "b _x A .. ."
/// bool(true)
/// int(5)
///
/// Warning: scandir(%s/nope): Failed to open directory: No such file or directory in %s on line 4
///
/// Warning: scandir(): (errno 2): No such file or directory in %s on line 4
///
/// Warning: scandir(%s/b): Failed to open directory: Not a directory in %s on line 4
///
/// Warning: scandir(): (errno 20): Not a directory in %s on line 4
/// bool(false)
/// bool(false)
/// scandir(): Argument #1 ($directory) must not be empty
/// ```
#[php_function]
fn scandir(
    req: &mut Request,
    directory: PathArg,
    #[php(default = SCANDIR_SORT_ASCENDING)] sorting_order: i64,
    #[php(default = null)] context: Option<Resource>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let PathArg(directory) = directory;
    let order = sorting_order;
    if directory.as_bytes().is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    // `if (zcontext) context = php_stream_context_from_zval(zcontext, 0);`
    let context = context_of(req, context, NullContext::None)?;
    let Some(stream) = req.with_stream_ctx(context, |req| req.open_dir(directory.as_bytes()))?
    else {
        // The errno the open left, which `php_stream_opendir` does not
        // hand back: the guard's `EPERM` where it refused, and otherwise
        // the stat of the path says which it was.
        let error = match req.check_open_basedir(directory.as_bytes(), false) {
            Err(refused) => refused.into(),
            Ok(path) => match std::fs::metadata(&path) {
                Ok(meta) if !meta.is_dir() => {
                    std::io::Error::from_raw_os_error(libc_errno::ENOTDIR)
                }
                Ok(_) => std::io::Error::from_raw_os_error(libc_errno::EACCES),
                Err(error) => error,
            },
        };
        let text = crate::filestat::strerror(&error);
        let code = error.raw_os_error().unwrap_or(0);
        docref(req, Level::Warning, &format!("(errno {code}): {text}"));
        return Ok(OrFalse::False);
    };
    let mut names: Vec<Vec<u8>> = Vec::new();
    loop {
        match stream.read_dir(req) {
            Ok(Some(name)) => names.push(name),
            Ok(None) => break,
            Err(thrown) => {
                stream.give_up(req);
                return Err(thrown);
            }
        }
    }
    stream.free(req);
    match order {
        SCANDIR_SORT_NONE => {}
        SCANDIR_SORT_ASCENDING => names.sort_by(|a, b| collate(a, b)),
        _ => names.sort_by(|a, b| collate(b, a)),
    }
    let mut array = PhpArray::new(req)?;
    for name in names {
        let name = PhpString::new(req, &name)?;
        array.append(req, name.into_value())?;
    }
    Ok(OrFalse::Value(array))
}

/// `glob(string $pattern, int $flags = 0): array|false`,
/// `PHP_FUNCTION(glob)` (`Z/ext/standard/dir.c:403`): php's warnings for
/// a pattern of `MAXPATHLEN` bytes or more and for a bit outside
/// `GLOB_AVAILABLE_FLAGS`; a relative pattern prefixed with the working
/// directory and the prefix taken off each path again, as php's ZTS
/// build does; then `php_glob` with `GLOB_ONLYDIR` masked off, no match
/// the empty array and any other failure `false`, and under
/// `GLOB_ONLYDIR` each path that `stat(2)` does not call a directory
/// dropped.
///
/// ```php
/// $d = __DIR__ . '/glob-d'; @mkdir($d); @mkdir("$d/sub"); foreach (['b.txt', 'a.txt', '.h', 'c.php'] as $f) touch("$d/$f");
/// chdir($d);
/// var_dump(glob('*.txt'), glob("$d/*.php") === ["$d/c.php"], glob('{a,c}*', GLOB_BRACE), glob('*', GLOB_ONLYDIR | GLOB_MARK), glob('nope*'), glob('nope*', GLOB_NOCHECK), glob('*', 1 << 20), glob(str_repeat('x', 1024)));
/// ```
/// ```expect
/// Warning: glob(): At least one of the passed flags is invalid or not supported on this platform in %s on line 4
///
/// Warning: glob(): Pattern exceeds the maximum allowed length of 1024 characters in %s on line 4
/// array(2) {
///   [0]=>
///   string(5) "a.txt"
///   [1]=>
///   string(5) "b.txt"
/// }
/// bool(true)
/// array(2) {
///   [0]=>
///   string(5) "a.txt"
///   [1]=>
///   string(5) "c.php"
/// }
/// array(1) {
///   [0]=>
///   string(4) "sub/"
/// }
/// array(0) {
/// }
/// array(1) {
///   [0]=>
///   string(5) "nope*"
/// }
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn glob(
    req: &mut Request,
    pattern: PathArg,
    #[php(default = 0)] flags: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let PathArg(pattern) = pattern;
    if pattern.len() >= MAXPATHLEN {
        let text = format!("Pattern exceeds the maximum allowed length of {MAXPATHLEN} characters");
        docref(req, Level::Warning, &text);
        return Ok(OrFalse::False);
    }
    let available = i64::from(php_glob::PHP_GLOB_AVAILABLE_FLAGS);
    if available & flags != flags {
        docref(
            req,
            Level::Warning,
            "At least one of the passed flags is invalid or not supported on this platform",
        );
        return Ok(OrFalse::False);
    }
    let mut work = pattern.as_bytes().to_vec();
    let mut cwd_skip = 0;
    if work.first() != Some(&b'/') {
        let cwd = req.cwd().as_os_str().as_bytes().to_vec();
        cwd_skip = cwd.len() + 1;
        let mut prefixed = cwd;
        prefixed.push(b'/');
        prefixed.extend_from_slice(&work);
        // `snprintf(work_pattern, MAXPATHLEN, ...)`.
        prefixed.truncate(MAXPATHLEN - 1);
        work = prefixed;
    }
    // Every available bit fits an `int`.
    let bits = i32::try_from(flags & i64::from(php_glob::PHP_GLOB_FLAGMASK)).unwrap_or(0);
    let paths = match php_glob::php_glob(&work, bits) {
        Ok(paths) => paths,
        Err(php_glob::PHP_GLOB_NOMATCH) => Vec::new(),
        Err(_) => return Ok(OrFalse::False),
    };
    if paths.is_empty() {
        return Ok(OrFalse::Value(Owner::take(req, PhpArray::empty())));
    }
    let only_dirs = flags & i64::from(php_glob::PHP_GLOB_ONLYDIR) != 0;
    let mut array = PhpArray::new(req)?;
    let mut basedir_limit = false;
    for path in paths {
        if req.check_open_basedir(&path, false).is_err() {
            basedir_limit = true;
            continue;
        }
        if only_dirs && !std::fs::metadata(host(&path)).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let shown = path.get(cwd_skip..).unwrap_or_default();
        let shown = PhpString::new(req, shown)?.into_value();
        array.append(req, shown)?;
    }
    // Every match refused by the guard is `false`, not the empty array.
    if basedir_limit && array.is_empty() {
        array.give_up(req);
        return Ok(OrFalse::False);
    }
    Ok(OrFalse::Value(array))
}
