//! `filestat.c`'s natives (`Z/ext/standard/filestat.c`) over the
//! engine's wrapper operations (design §4.3 seam 5): the `file*` and
//! `is_*` family, `stat` and `lstat`, all `php_stat` over the path's
//! wrapper's `url_stat` and the stat cache php keeps in
//! `BG(CurrentStatFile)`, which the engine holds since its plain files
//! kind clears it; `chown`, `chgrp`, `lchown`, `lchgrp`, `chmod` and
//! `touch`, which call the C library themselves for a plain path and
//! hand any other, a `file://` one included, to the wrapper's
//! `stream_metadata`; and `clearstatcache`. In the stub's order
//! (`basic_functions.stub.php`). `access(2)` and the rest `std` has no
//! answer for are `ffi`'s.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use php_ext::coerce::coerce_param;
use php_ext::decl::{ConstExpr, FunctionDecl, Parameter, Type};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw};
use php_ext::request::{Request, WrapperOp};
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};

use crate::ffi;
use crate::output::Entry;
use crate::{argument_count_between, bool_argument, declared, long_argument, parameter};

/// `S_IFMT` and the file types `filetype` names, with `S_IFLNK`, which
/// `php_stat` tests before the rest.
const S_IFMT: i64 = 0o170_000;
const S_IFIFO: i64 = 0o010_000;
const S_IFCHR: i64 = 0o020_000;
const S_IFDIR: i64 = 0o040_000;
const S_IFBLK: i64 = 0o060_000;
const S_IFREG: i64 = 0o100_000;
const S_IFLNK: i64 = 0o120_000;
const S_IFSOCK: i64 = 0o140_000;

/// The names `php_stat` gives the thirteen fields after their numbers
/// (`Z/ext/standard/filestat.c:957`).
const STAT_NAMES: [&[u8]; 13] = [
    b"dev", b"ino", b"mode", b"nlink", b"uid", b"gid", b"rdev", b"size", b"atime", b"mtime",
    b"ctime", b"blksize", b"blocks",
];

/// `FS_*` (`Z/ext/standard/php_filestat.h:47`): what `php_stat` is
/// asked of a path.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fs {
    Perms,
    Inode,
    Size,
    Owner,
    Group,
    Atime,
    Mtime,
    Ctime,
    Type,
    IsW,
    IsR,
    IsX,
    IsFile,
    IsDir,
    IsLink,
    Exists,
    Lstat,
    Stat,
}

impl Fs {
    /// `IS_LINK_OPERATION`: stated with `lstat(2)`.
    fn is_link_operation(self) -> bool {
        matches!(self, Fs::Type | Fs::IsLink | Fs::Lstat)
    }

    /// `IS_EXISTS_CHECK`: a `bool` whose failure says nothing.
    fn is_exists_check(self) -> bool {
        matches!(
            self,
            Fs::Exists | Fs::IsW | Fs::IsR | Fs::IsX | Fs::IsFile | Fs::IsDir | Fs::IsLink
        )
    }

    /// `IS_ABLE_CHECK`.
    fn is_able_check(self) -> bool {
        matches!(self, Fs::IsR | Fs::IsW | Fs::IsX)
    }

    /// `IS_ACCESS_CHECK`: what `access(2)` answers of a plain file.
    fn is_access_check(self) -> bool {
        self.is_able_check() || self == Fs::Exists
    }
}

/// The natives declared here, in the stub's order.
///
/// ```php
/// echo new ReflectionFunction('touch'), new ReflectionFunction('chown'), new ReflectionFunction('clearstatcache');
/// ```
/// ```expect
/// Function [ <internal:standard> function touch ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $filename ]
///     Parameter #1 [ <optional> ?int $mtime = null ]
///     Parameter #2 [ <optional> ?int $atime = null ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function chown ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $filename ]
///     Parameter #1 [ <required> string|int $user ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function clearstatcache ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <optional> bool $clear_realpath_cache = false ]
///     Parameter #1 [ <optional> string $filename = "" ]
///   }
///   - Return [ void ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let int_or_false = || Type::Union(vec![Type::Int, Type::False]);
    let filename = || parameter(b"filename", Type::String);
    let stat_one = |name: &[u8], returns: Type| declared(name, &[filename()], returns);
    let owner = |name: &[u8], who: &[u8]| {
        declared(
            name,
            &[
                filename(),
                parameter(who, Type::Union(vec![Type::String, Type::Int])),
            ],
            Type::Bool,
        )
    };
    let nullable_int = |name: &[u8]| Parameter {
        default: Some(ConstExpr::Null),
        ..parameter(name, Type::Nullable(Box::new(Type::Int)))
    };
    vec![
        (stat_one(b"fileatime", int_or_false()), fileatime as Entry),
        (stat_one(b"filectime", int_or_false()), filectime),
        (stat_one(b"filegroup", int_or_false()), filegroup),
        (stat_one(b"fileinode", int_or_false()), fileinode),
        (stat_one(b"filemtime", int_or_false()), filemtime),
        (stat_one(b"fileowner", int_or_false()), fileowner),
        (stat_one(b"fileperms", int_or_false()), fileperms),
        (stat_one(b"filesize", int_or_false()), filesize),
        (
            stat_one(b"filetype", Type::Union(vec![Type::String, Type::False])),
            filetype,
        ),
        (stat_one(b"file_exists", Type::Bool), file_exists),
        (stat_one(b"is_writable", Type::Bool), is_writable),
        (stat_one(b"is_writeable", Type::Bool), is_writable),
        (stat_one(b"is_readable", Type::Bool), is_readable),
        (stat_one(b"is_executable", Type::Bool), is_executable),
        (stat_one(b"is_file", Type::Bool), is_file),
        (stat_one(b"is_dir", Type::Bool), is_dir),
        (stat_one(b"is_link", Type::Bool), is_link),
        (
            stat_one(b"stat", Type::Union(vec![Type::Array, Type::False])),
            stat,
        ),
        (
            stat_one(b"lstat", Type::Union(vec![Type::Array, Type::False])),
            lstat,
        ),
        (owner(b"chown", b"user"), chown),
        (owner(b"chgrp", b"group"), chgrp),
        (owner(b"lchown", b"user"), lchown),
        (owner(b"lchgrp", b"group"), lchgrp),
        (
            declared(
                b"chmod",
                &[filename(), parameter(b"permissions", Type::Int)],
                Type::Bool,
            ),
            chmod,
        ),
        (
            declared(
                b"touch",
                &[filename(), nullable_int(b"mtime"), nullable_int(b"atime")],
                Type::Bool,
            ),
            touch,
        ),
        (
            declared(
                b"clearstatcache",
                &[
                    Parameter {
                        default: Some(ConstExpr::Bool(false)),
                        ..parameter(b"clear_realpath_cache", Type::Bool)
                    },
                    Parameter {
                        default: Some(ConstExpr::Str(Box::new([]))),
                        ..parameter(b"filename", Type::String)
                    },
                ],
                Type::Void,
            ),
            clearstatcache,
        ),
    ]
}

/// `strerror(errno)`: the text Rust's error carries before its own
/// ` (os error N)`.
pub(super) fn strerror(error: &std::io::Error) -> String {
    let text = error.to_string();
    text.split_once(" (os error")
        .map_or(text.clone(), |(text, _)| text.to_owned())
}

/// A path as the C library is handed it: against the request's working
/// directory, and the empty path left empty, which every call answers
/// `ENOENT` to.
fn host(req: &Request, path: &[u8]) -> PathBuf {
    if path.is_empty() {
        PathBuf::new()
    } else {
        req.absolutize(path).0
    }
}

/// The permission bits of a file `fopen(3)` made, `0666 & ~umask` as the
/// request's `umask()` has it, whatever the process's own mask left.
fn apply_umask(req: &Request, path: &std::path::Path) {
    let want = 0o666 & !req.umask();
    if let Ok(meta) = std::fs::metadata(path) {
        let have = meta.permissions().mode();
        if have & 0o777 != want {
            let _ = std::fs::set_permissions(
                path,
                std::fs::Permissions::from_mode((have & 0o7000) | want),
            );
        }
    }
}

/// `Z_PARAM_LONG_OR_NULL`: the argument at `position`, `None` where it
/// is `null` or not passed, else coerced as an int parameter.
pub(super) fn nullable_long(req: &mut Request, position: u32) -> Result<Option<i64>, Thrown> {
    match req.argument(position) {
        Some(given) if !matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) => {
            let coerced =
                coerce_param(req, &given, &Type::Nullable(Box::new(Type::Int)), position)?;
            Ok(coerced.as_int())
        }
        _ => Ok(None),
    }
}

/// `Z_PARAM_PATH`: the string argument at `position`, with php's
/// `ValueError` for a null byte in it.
fn path_argument(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let path = crate::string_argument(req, position)?;
    if path.as_bytes().contains(&0) {
        return Err(arg_value_error(req, position, "not contain any null bytes"));
    }
    Ok(path)
}

/// Whether `touch`, `chmod`, `chown` and `chgrp` call the C library
/// themselves: the path is the plain files wrapper's and not written
/// with `file://`, `wrapper != &php_plain_files_wrapper ||
/// strncasecmp("file://", filename, 7) == 0` read the other way.
fn is_plain_direct(req: &mut Request, filename: &[u8]) -> bool {
    req.plain_path(filename).is_some() && !has_file_scheme(filename)
}

/// `strncasecmp("file://", filename, 7) == 0`.
fn has_file_scheme(filename: &[u8]) -> bool {
    filename.len() >= 7 && filename[..7].eq_ignore_ascii_case(b"file://")
}

/// `php_stat` (`Z/ext/standard/filestat.c:743`): an access check of a
/// plain file is `access(2)`, and nothing is cached; anything else is
/// the path's stat through its wrapper, from the stat cache where it
/// holds the path as written and into it after, `lstat(2)` for the link
/// operations. A path that is empty or holds a NUL byte is `false`, with
/// php's warning for a NUL where the answer is not a `bool`; a stat that
/// failed is `false`, with `stat failed for` or `Lstat failed for`
/// where it is not. An access check of another wrapper's file reads the
/// owner's, group's or other's permission bits by whose the file is.
fn php_stat(req: &mut Request, filename: &[u8], ty: Fs) -> Result<Value, Thrown> {
    let bad = filename.is_empty() || filename.contains(&0);
    if ty.is_access_check() {
        if bad {
            return Ok(Value::bool(false));
        }
        if let Some(local) = req.plain_path(filename) {
            let mode = match ty {
                Fs::IsW => ffi::W_OK,
                Fs::IsR => ffi::R_OK,
                Fs::IsX => ffi::X_OK,
                _ => ffi::F_OK,
            };
            return Ok(Value::bool(ffi::access(&host(req, &local), mode)));
        }
    }
    if bad {
        if !filename.is_empty() && !ty.is_exists_check() {
            docref(req, Level::Warning, "Filename contains null byte");
        }
        return Ok(Value::bool(false));
    }
    let link = ty.is_link_operation();
    let answered = if ty.is_exists_check() {
        req.url_stat_quiet(filename, link)?
    } else {
        req.url_stat(filename, link, true)?
    };
    let Some(sb) = answered else {
        if !ty.is_exists_check() {
            let text = format!(
                "{}stat failed for {}",
                if link { "L" } else { "" },
                String::from_utf8_lossy(filename)
            );
            docref(req, Level::Warning, &text);
        }
        return Ok(Value::bool(false));
    };
    let mode = sb[2];
    let (mut rmask, mut wmask, mut xmask) = (0o4, 0o2, 0o1);
    if ty.is_able_check() {
        let group = || (0o40, 0o20, 0o10);
        if sb[4] == i64::from(ffi::uid()) {
            (rmask, wmask, xmask) = (0o400, 0o200, 0o100);
        } else if sb[5] == i64::from(ffi::gid())
            || ffi::groups().iter().any(|gid| sb[5] == i64::from(*gid))
        {
            (rmask, wmask, xmask) = group();
        }
    }
    Ok(match ty {
        Fs::Perms => Value::int(mode),
        Fs::Inode => Value::int(sb[1]),
        Fs::Size => Value::int(sb[7]),
        Fs::Owner => Value::int(sb[4]),
        Fs::Group => Value::int(sb[5]),
        Fs::Atime => Value::int(sb[8]),
        Fs::Mtime => Value::int(sb[9]),
        Fs::Ctime => Value::int(sb[10]),
        Fs::Type => {
            let name: &[u8] = match mode & S_IFMT {
                S_IFLNK => b"link",
                S_IFIFO => b"fifo",
                S_IFCHR => b"char",
                S_IFDIR => b"dir",
                S_IFBLK => b"block",
                S_IFREG => b"file",
                S_IFSOCK => b"socket",
                other => {
                    let text = format!("Unknown file type ({other})");
                    docref(req, Level::Notice, &text);
                    b"unknown"
                }
            };
            Value::string(PhpString::new(req, name)?)
        }
        Fs::IsW => Value::bool(mode & wmask != 0),
        Fs::IsR => Value::bool(mode & rmask != 0),
        Fs::IsX => Value::bool(mode & xmask != 0),
        Fs::IsFile => Value::bool(mode & S_IFMT == S_IFREG),
        Fs::IsDir => Value::bool(mode & S_IFMT == S_IFDIR),
        Fs::IsLink => Value::bool(mode & S_IFMT == S_IFLNK),
        Fs::Exists => Value::bool(true),
        Fs::Lstat | Fs::Stat => {
            let mut out = PhpArray::new(req)?;
            for field in sb {
                out.append(req, Value::int(field))?;
            }
            for (name, field) in STAT_NAMES.iter().zip(sb) {
                let key = Key::Str(PhpString::new(req, name)?);
                out.set(req, key, Value::int(field))?;
            }
            Value::array(out)
        }
    })
}

/// `FileFunction(name, funcnum)` (`Z/ext/standard/filestat.c:1016`): one
/// `Z_PARAM_STR` and `php_stat` over it.
fn file_function(req: &mut Request, ty: Fs) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let filename = crate::string_argument(req, 1)?;
    php_stat(req, filename.as_bytes(), ty)
}

/// `fileatime(string $filename): int|false`: `st_atime`, or `false` after
/// php's `stat failed for` warning.
///
/// ```php
/// $p = __DIR__ . '/a.txt'; touch($p, 1000, 2000);
/// var_dump(fileatime($p), fileatime(__DIR__ . '/nope'));
/// ```
/// ```expect
/// Warning: fileatime(): stat failed for %s/nope in %s on line 3
/// int(2000)
/// bool(false)
/// ```
fn fileatime(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::Atime)
}

/// `filectime(string $filename): int|false`: `st_ctime`; a wrapper with
/// no `url_stat`, `php://` among them, is a stat that failed.
///
/// ```php
/// var_dump(filectime(__FILE__) > 0, filectime('php://memory'), filectime("a\0b"));
/// ```
/// ```expect
/// Warning: filectime(): stat failed for php://memory in %s on line 2
///
/// Warning: filectime(): Filename contains null byte in %s on line 2
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
fn filectime(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::Ctime)
}

/// `filegroup(string $filename): int|false`: `st_gid`.
///
/// ```php
/// var_dump(filegroup(__FILE__) === stat(__FILE__)['gid'], filegroup(''));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
fn filegroup(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::Group)
}

/// `fileinode(string $filename): int|false`: `st_ino`.
///
/// ```php
/// var_dump(fileinode(__FILE__) === stat(__FILE__)['ino'], fileinode(__FILE__) > 0);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
fn fileinode(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::Inode)
}

/// `filemtime(string $filename): int|false`: `st_mtime`, from the stat
/// cache where it holds the path, which `touch` clears.
///
/// ```php
/// $p = __DIR__ . '/m.txt'; touch($p, 1000);
/// var_dump(filemtime($p)); touch($p, 5000); var_dump(filemtime($p));
/// ```
/// ```expect
/// int(1000)
/// int(5000)
/// ```
fn filemtime(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::Mtime)
}

/// `fileowner(string $filename): int|false`: `st_uid`.
///
/// ```php
/// var_dump(fileowner(__FILE__) === stat(__FILE__)['uid'], fileowner(__DIR__ . '/nope'));
/// ```
/// ```expect
/// Warning: fileowner(): stat failed for %s/nope in %s on line 2
/// bool(true)
/// bool(false)
/// ```
fn fileowner(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::Owner)
}

/// `fileperms(string $filename): int|false`: `st_mode`, the file type
/// bits with the permissions.
///
/// ```php
/// $p = __DIR__ . '/p.txt'; touch($p); chmod($p, 0640);
/// printf("%o %o\n", fileperms($p), fileperms(__DIR__) & 0170000);
/// ```
/// ```expect
/// 100640 40000
/// ```
fn fileperms(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::Perms)
}

/// `filesize(string $filename): int|false`: `st_size`. A write to the
/// file through a stream clears the stat cache, so the size read after
/// it is the new one.
///
/// ```php
/// $p = __DIR__ . '/s.txt'; file_put_contents($p, "hello");
/// var_dump(filesize($p)); $f = fopen($p, 'a'); fwrite($f, "!!"); var_dump(filesize($p));
/// var_dump(filesize(__DIR__ . '/nope'));
/// ```
/// ```expect
/// int(5)
/// int(7)
///
/// Warning: filesize(): stat failed for %s/nope in %s on line 4
/// bool(false)
/// ```
fn filesize(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::Size)
}

/// `filetype(string $filename): string|false`: the type `lstat(2)`
/// reads, `link` for a symbolic link, and php's `Lstat failed for`
/// where there is none.
///
/// ```php
/// $d = __DIR__; touch("$d/t.txt"); shell_exec("ln -s $d/t.txt $d/t.lnk");
/// var_dump(filetype("$d/t.txt"), filetype($d), filetype("$d/t.lnk"), filetype("$d/nope"));
/// ```
/// ```expect
/// Warning: filetype(): Lstat failed for %s/nope in %s on line 3
/// string(4) "file"
/// string(3) "dir"
/// string(4) "link"
/// bool(false)
/// ```
fn filetype(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::Type)
}

/// `file_exists(string $filename): bool`: `access(F_OK)` for a plain
/// file, and a stat that says nothing where it fails for any other.
///
/// ```php
/// var_dump(file_exists(__FILE__), file_exists(__DIR__), file_exists(__DIR__ . '/nope'), file_exists(''), file_exists("a\0b"), file_exists('php://memory'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn file_exists(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::Exists)
}

/// `is_writable(string $filename): bool` and its alias `is_writeable`:
/// `access(W_OK)` for a plain file.
///
/// ```php
/// $p = __DIR__ . '/w.txt'; touch($p); chmod($p, 0444);
/// var_dump(is_writable($p), is_writeable($p)); chmod($p, 0644); var_dump(is_writable($p), is_writable(__DIR__ . '/nope'));
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
fn is_writable(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::IsW)
}

/// `is_readable(string $filename): bool`: `access(R_OK)` for a plain
/// file.
///
/// ```php
/// $p = __DIR__ . '/r.txt'; touch($p); chmod($p, 0200);
/// var_dump(is_readable($p)); chmod($p, 0644); var_dump(is_readable($p), is_readable('php://memory'));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
fn is_readable(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::IsR)
}

/// `is_executable(string $filename): bool`: `access(X_OK)` for a plain
/// file.
///
/// ```php
/// $p = __DIR__ . '/x.sh'; touch($p); chmod($p, 0644);
/// var_dump(is_executable($p)); chmod($p, 0755); var_dump(is_executable($p), is_executable(__DIR__));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
fn is_executable(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::IsX)
}

/// `is_file(string $filename): bool`: a regular file, by `stat(2)`.
///
/// ```php
/// var_dump(is_file(__FILE__), is_file(__DIR__), is_file(__DIR__ . '/nope'), is_file("file://" . __DIR__ . '/witness.php'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
fn is_file(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::IsFile)
}

/// `is_dir(string $filename): bool`: a directory, by `stat(2)`.
///
/// ```php
/// var_dump(is_dir(__DIR__), is_dir(__FILE__), is_dir(''), is_dir(__DIR__ . '/nope'), is_dir('.'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
fn is_dir(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::IsDir)
}

/// `is_link(string $filename): bool`: a symbolic link, by `lstat(2)`.
///
/// ```php
/// $d = __DIR__; touch("$d/l.txt"); shell_exec("ln -s $d/l.txt $d/l.lnk");
/// var_dump(is_link("$d/l.lnk"), is_link("$d/l.txt"), is_link("$d/nope"));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
fn is_link(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::IsLink)
}

/// `stat(string $filename): array|false`: the thirteen fields under
/// `0` to `12` and then under their names, as `fstat` lays them out.
///
/// ```php
/// $p = __DIR__ . '/st.txt'; file_put_contents($p, "abc"); $s = stat($p);
/// foreach ($s as $k => $v) { echo $k, ','; } echo "\n";
/// var_dump(count($s), $s['size'], $s[7], $s['mode'] & 0170000, stat(__DIR__ . '/nope'));
/// ```
/// ```expect
/// 0,1,2,3,4,5,6,7,8,9,10,11,12,dev,ino,mode,nlink,uid,gid,rdev,size,atime,mtime,ctime,blksize,blocks,
///
/// Warning: stat(): stat failed for %s/nope in %s on line 4
/// int(26)
/// int(3)
/// int(3)
/// int(32768)
/// bool(false)
/// ```
fn stat(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::Stat)
}

/// `lstat(string $filename): array|false`: `stat`'s fields of the link
/// itself; php's `Lstat failed for` where there is none.
///
/// ```php
/// $d = __DIR__; file_put_contents("$d/ls.txt", "abcdef"); shell_exec("ln -s $d/ls.txt $d/ls.lnk");
/// var_dump(lstat("$d/ls.lnk")['mode'] & 0170000, stat("$d/ls.lnk")['size'], lstat("$d/ls.txt")['size'], lstat("$d/nope"));
/// ```
/// ```expect
/// Warning: lstat(): Lstat failed for %s/nope in %s on line 3
/// int(40960)
/// int(6)
/// int(6)
/// bool(false)
/// ```
fn lstat(req: &mut Request) -> Result<Value, Thrown> {
    file_function(req, Fs::Lstat)
}

/// A user or a group as `Z_PARAM_STR_OR_LONG` reads one.
enum Who {
    Name(PhpString),
    Id(i64),
}

/// `Z_PARAM_STR_OR_LONG` at `position`: an int or a string as it is,
/// anything else coerced to `string|int` as a parameter of that type.
fn who_argument(req: &mut Request, position: u32) -> Result<Who, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let value = match given.deref().as_ref() {
        ValueRef::Int(_) | ValueRef::Str(_) => given.deref(),
        _ => coerce_param(
            req,
            &given,
            &Type::Union(vec![Type::String, Type::Int]),
            position,
        )?,
    };
    Ok(match value.as_ref() {
        ValueRef::Str(s) => Who::Name(*s),
        _ => Who::Id(value.as_int().unwrap_or(0)),
    })
}

/// `php_do_chown` and `php_do_chgrp` (`Z/ext/standard/filestat.c:459`,
/// `:318`): a path that is not a plain one, or is written `file://`, is
/// the wrapper's `stream_metadata`, a name looked up first for the plain
/// files wrapper's own; otherwise the name looked up, php's `Unable to
/// find uid for %s` or `gid` where there is none, then `chown(2)` or
/// `lchown(2)` with the other id left alone, php's warning with
/// `strerror(errno)` where it failed, and the stat cache cleared.
fn owner_change(req: &mut Request, group: bool, link: bool) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let filename = path_argument(req, 1)?;
    let filename = filename.as_bytes();
    let who = who_argument(req, 2)?;
    let kind = if group { "gid" } else { "uid" };
    let lookup = |name: &[u8]| {
        if group {
            ffi::gid_by_name(name)
        } else {
            ffi::uid_by_name(name)
        }
    };
    let plain = req.plain_path(filename).is_some();
    if !plain || has_file_scheme(filename) {
        let id = match who {
            Who::Id(id) => id,
            Who::Name(name) if !plain => {
                // `php_do_chown` hands another wrapper the name as it was
                // given, `PHP_STREAM_META_OWNER_NAME`.
                let op = if group {
                    WrapperOp::GroupName(name.as_bytes())
                } else {
                    WrapperOp::OwnerName(name.as_bytes())
                };
                return Ok(Value::bool(req.wrapper_op(filename, op)?));
            }
            Who::Name(name) => {
                // The plain files wrapper's `php_plain_files_metadata`
                // looks the name up, and says so by the path it was
                // handed, `file://` taken off.
                let Some(id) = lookup(name.as_bytes()) else {
                    let native = match (group, link) {
                        (false, false) => "chown",
                        (true, false) => "chgrp",
                        (false, true) => "lchown",
                        (true, true) => "lchgrp",
                    };
                    let text = format!(
                        "{native}({}): Unable to find {kind} for {}",
                        String::from_utf8_lossy(&filename[7..]),
                        String::from_utf8_lossy(name.as_bytes())
                    );
                    php_ext::raising::warning(req, &text);
                    return Ok(Value::bool(false));
                };
                i64::from(id)
            }
        };
        let op = if group {
            WrapperOp::Group(id)
        } else {
            WrapperOp::Owner(id)
        };
        return Ok(Value::bool(req.wrapper_op(filename, op)?));
    }
    let id = match who {
        Who::Id(id) => u32::try_from(id & 0xFFFF_FFFF).unwrap_or(u32::MAX),
        Who::Name(name) => {
            let Some(id) = lookup(name.as_bytes()) else {
                let text = format!(
                    "Unable to find {kind} for {}",
                    String::from_utf8_lossy(name.as_bytes())
                );
                docref(req, Level::Warning, &text);
                return Ok(Value::bool(false));
            };
            id
        }
    };
    let (uid, gid) = if group {
        (None, Some(id))
    } else {
        (Some(id), None)
    };
    let path = host(req, filename);
    let done = if link {
        std::os::unix::fs::lchown(&path, uid, gid)
    } else {
        std::os::unix::fs::chown(&path, uid, gid)
    };
    if let Err(error) = done {
        docref(req, Level::Warning, &strerror(&error));
        return Ok(Value::bool(false));
    }
    req.clear_stat_cache();
    Ok(Value::bool(true))
}

/// `chown(string $filename, string|int $user): bool`,
/// `PHP_FUNCTION(chown)` (`Z/ext/standard/filestat.c:543`): the file's
/// owner, by id or by name; a wrapper that is not the plain files one is
/// handed the name as it was given, `STREAM_META_OWNER_NAME`, and an id
/// as `STREAM_META_OWNER`.
///
/// ```php
/// class Meta { public $context;
///     function stream_metadata($path, $option, $value) { echo "$path $option $value\n"; return true; } }
/// stream_wrapper_register('meta', 'Meta');
/// var_dump(chown('meta://a', 'someone'), chown('meta://b', 42), chgrp('meta://c', 'staff'), chgrp('meta://d', 7));
/// ```
/// ```expect
/// meta://a 2 someone
/// meta://b 3 42
/// meta://c 4 staff
/// meta://d 5 7
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
///
/// ```php
/// $p = __DIR__ . '/o.txt'; touch($p); $me = fileowner($p);
/// var_dump(chown($p, $me), chown($p, 'no_such_user_here'), chown(__DIR__ . '/nope', $me), chown('php://memory', $me), chown("file://$p", $me));
/// ```
/// ```expect
/// Warning: chown(): Unable to find uid for no_such_user_here in %s on line 3
///
/// Warning: chown(): No such file or directory in %s on line 3
///
/// Warning: chown(): Cannot call chown() for a non-standard stream in %s on line 3
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
fn chown(req: &mut Request) -> Result<Value, Thrown> {
    owner_change(req, false, false)
}

/// `chgrp(string $filename, string|int $group): bool`,
/// `PHP_FUNCTION(chgrp)` (`Z/ext/standard/filestat.c:400`): the file's
/// group, by id or by name.
///
/// ```php
/// $p = __DIR__ . '/g.txt'; touch($p); $g = filegroup($p);
/// var_dump(chgrp($p, $g), chgrp($p, 'no_such_group_here'), chgrp(__DIR__ . '/nope', $g), chgrp('php://memory', $g));
/// ```
/// ```expect
/// Warning: chgrp(): Unable to find gid for no_such_group_here in %s on line 3
///
/// Warning: chgrp(): No such file or directory in %s on line 3
///
/// Warning: chgrp(): Cannot call chgrp() for a non-standard stream in %s on line 3
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn chgrp(req: &mut Request) -> Result<Value, Thrown> {
    owner_change(req, true, false)
}

/// `lchown(string $filename, string|int $user): bool`: `chown` of a
/// symbolic link itself, `lchown(2)`.
///
/// ```php
/// $d = __DIR__; touch("$d/lo.txt"); shell_exec("ln -s $d/lo.txt $d/lo.lnk");
/// var_dump(lchown("$d/lo.lnk", fileowner("$d/lo.txt")), lchown("$d/nope", 0));
/// ```
/// ```expect
/// Warning: lchown(): No such file or directory in %s on line 3
/// bool(true)
/// bool(false)
/// ```
fn lchown(req: &mut Request) -> Result<Value, Thrown> {
    owner_change(req, false, true)
}

/// `lchgrp(string $filename, string|int $group): bool`: `chgrp` of a
/// symbolic link itself.
///
/// ```php
/// $d = __DIR__; touch("$d/lg.txt"); shell_exec("ln -s $d/lg.txt $d/lg.lnk");
/// var_dump(lchgrp("$d/lg.lnk", filegroup("$d/lg.txt")), lchgrp('php://memory', 0));
/// ```
/// ```expect
/// Warning: lchgrp(): Cannot call chgrp() for a non-standard stream in %s on line 3
/// bool(true)
/// bool(false)
/// ```
fn lchgrp(req: &mut Request) -> Result<Value, Thrown> {
    owner_change(req, true, true)
}

/// `chmod(string $filename, int $permissions): bool`,
/// `PHP_FUNCTION(chmod)` (`Z/ext/standard/filestat.c:560`): `chmod(2)`
/// of a plain path, php's warning with `strerror(errno)` where it
/// failed, and the stat cache cleared; any other path, a `file://` one
/// included, is the wrapper's `stream_metadata`.
///
/// ```php
/// $p = __DIR__ . '/c.txt'; touch($p);
/// var_dump(chmod($p, 0600)); printf("%o\n", fileperms($p));
/// var_dump(chmod("file://$p", 0640)); printf("%o\n", fileperms($p));
/// var_dump(chmod(__DIR__ . '/nope', 0600), chmod('file://' . __DIR__ . '/nope', 0600), chmod('php://memory', 0600));
/// ```
/// ```expect
/// bool(true)
/// 100600
/// bool(true)
/// 100640
///
/// Warning: chmod(): No such file or directory in %s on line 5
///
/// Warning: chmod(%s/nope): Operation failed: No such file or directory in %s on line 5
///
/// Warning: chmod(): Cannot call chmod() for a non-standard stream in %s on line 5
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn chmod(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let filename = path_argument(req, 1)?;
    let filename = filename.as_bytes();
    let mode = long_argument(req, 2)?;
    if !is_plain_direct(req, filename) {
        return Ok(Value::bool(
            req.wrapper_op(filename, WrapperOp::Chmod(mode))?,
        ));
    }
    let bits = u32::try_from(mode & 0o7777).unwrap_or(0);
    if let Err(error) =
        std::fs::set_permissions(host(req, filename), std::fs::Permissions::from_mode(bits))
    {
        docref(req, Level::Warning, &strerror(&error));
        return Ok(Value::bool(false));
    }
    req.clear_stat_cache();
    Ok(Value::bool(true))
}

/// `touch(string $filename, ?int $mtime = null, ?int $atime = null): bool`,
/// `PHP_FUNCTION(touch)` (`Z/ext/standard/filestat.c:609`): no times is
/// the current time, an `$mtime` alone both, and an `$atime` alone php's
/// `ValueError`; a plain path is made where `access(F_OK)` finds none,
/// `0666` under the `umask`, and given the times by `utime(2)`, each
/// failure php's warning; any other path, a `file://` one included, is
/// the wrapper's `stream_metadata`, and a wrapper with none is opened
/// `c` and closed where no time was given.
///
/// ```php
/// $p = __DIR__ . '/new.txt';
/// var_dump(file_exists($p), touch($p), file_exists($p), touch($p, 100, 200), filemtime($p), fileatime($p), touch($p, 300), fileatime($p));
/// var_dump(touch("file://$p", 400), filemtime($p), touch(''), touch(__DIR__ . '/nodir/x'), touch('php://memory'), touch('php://memory', 1));
/// try { touch($p, null, 5); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// int(100)
/// int(200)
/// bool(true)
/// int(300)
///
/// Warning: touch(): Unable to create file %s/nodir/x because No such file or directory in %s on line 4
///
/// Warning: touch(): Cannot call touch() for a non-standard stream in %s on line 4
/// bool(true)
/// int(400)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// touch(): Argument #2 ($mtime) cannot be null when argument #3 ($atime) is an integer
/// ```
fn touch(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 3)?;
    let filename = path_argument(req, 1)?;
    let filename = filename.as_bytes();
    let mtime = nullable_long(req, 2)?;
    let atime = nullable_long(req, 3)?;
    if filename.is_empty() {
        return Ok(Value::bool(false));
    }
    let times = match (mtime, atime) {
        (None, None) => None,
        (Some(mtime), None) => Some((mtime, mtime)),
        (None, Some(_)) => {
            // `zend_argument_value_error`, whose message carries no
            // `must` of its own.
            return Err(throw(
                req,
                "ValueError",
                "touch(): Argument #2 ($mtime) cannot be null when argument #3 ($atime) is an integer",
            ));
        }
        (Some(mtime), Some(atime)) => Some((mtime, atime)),
    };
    if !is_plain_direct(req, filename) {
        return Ok(Value::bool(
            req.wrapper_op(filename, WrapperOp::Touch(times))?,
        ));
    }
    let path = host(req, filename);
    if !ffi::access(&path, ffi::F_OK) {
        if let Err(error) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
        {
            let text = format!(
                "Unable to create file {} because {}",
                String::from_utf8_lossy(filename),
                strerror(&error)
            );
            docref(req, Level::Warning, &text);
            return Ok(Value::bool(false));
        }
        apply_umask(req, &path);
    }
    if let Err(error) = ffi::utime(&path, times) {
        let text = format!("Utime failed: {}", strerror(&error));
        docref(req, Level::Warning, &text);
        return Ok(Value::bool(false));
    }
    req.clear_stat_cache();
    Ok(Value::bool(true))
}

/// `clearstatcache(bool $clear_realpath_cache = false, string $filename = ""): void`,
/// `PHP_FUNCTION(clearstatcache)` (`Z/ext/standard/filestat.c:721`):
/// both entries of the stat cache dropped, whatever is asked; the
/// realpath cache is not modelled. Until it is cleared, a stat of a path
/// answers what it answered last, though `mkdir` changed the directory
/// under it since, as `mkdir` does not clear the cache.
///
/// ```php
/// $d = __DIR__; $n = stat($d)['nlink']; mkdir("$d/sub");
/// var_dump(stat($d)['nlink'] === $n); clearstatcache(); var_dump(stat($d)['nlink'] === $n); clearstatcache(true, $d);
/// try { clearstatcache(false, "a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// clearstatcache(): Argument #2 ($filename) must not contain any null bytes
/// ```
fn clearstatcache(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 2)?;
    if req.argument_count() >= 1 {
        bool_argument(req, 1)?;
    }
    if req.argument_count() >= 2 {
        path_argument(req, 2)?;
    }
    req.clear_stat_cache();
    Ok(Value::null())
}
