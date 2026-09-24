//! `spl_directory.c`: `SplFileInfo`, `DirectoryIterator`,
//! `FilesystemIterator`, `RecursiveDirectoryIterator`, `GlobIterator`,
//! `SplFileObject` and `SplTempFileObject` over one payload, php's
//! `spl_filesystem_object` ([`Fs`]), and the engine's streams layer: a
//! directory is a stream `opendir` opens and `readdir` reads, a file a
//! stream `fopen` opens, and a stat the wrapper's `url_stat`. The CSV
//! methods and `GlobIterator`'s pattern hand their work to
//! `ext/standard`'s `fgetcsv`, `fputcsv` and `glob` by name, as
//! `spl_directory.c` calls `php_fgetcsv`, `php_fputcsv` and the `glob://`
//! wrapper.

use php_ext::abi::{CloneObj, ObjectHandlers};
use php_ext::call::{call_method, instantiate, lookup_class, new_object};
use php_ext::raising::{Thrown, arg_type_error, arg_value_error, throw};
use php_ext::request::{Request, Stream, Whence};
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::Entry;
use crate::stub::{self, Declared};

const CURRENT_AS_PATHNAME: i64 = 0x20;
const CURRENT_AS_FILEINFO: i64 = 0;
const CURRENT_AS_SELF: i64 = 0x10;
const CURRENT_MODE_MASK: i64 = 0xF0;
const KEY_AS_PATHNAME: i64 = 0;
const KEY_AS_FILENAME: i64 = 0x100;
const FOLLOW_SYMLINKS: i64 = 0x4000;
const KEY_MODE_MASK: i64 = 0xF00;
const SKIP_DOTS: i64 = 0x1000;
const UNIX_PATHS: i64 = 0x2000;
const OTHER_MODE_MASK: i64 = 0x7000;
const DROP_NEW_LINE: i64 = 1;
const READ_AHEAD: i64 = 2;
const SKIP_EMPTY: i64 = 4;
const READ_CSV: i64 = 8;

/// `SPL_FS_INFO`, `SPL_FS_DIR` and `SPL_FS_FILE`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Type {
    #[default]
    Info,
    Dir,
    File,
}

/// `spl_filesystem_object`.
#[derive(Default)]
pub(crate) struct Fs {
    kind: Type,
    path: Option<Vec<u8>>,
    file_name: Option<Vec<u8>>,
    flags: i64,
    file_class: Option<ClassId>,
    info_class: Option<ClassId>,
    dir: Option<Stream>,
    entry: Vec<u8>,
    index: i64,
    sub_path: Option<Vec<u8>>,
    glob: Option<(Vec<u8>, Vec<Vec<u8>>)>,
    stream: Option<Stream>,
    open_mode: Vec<u8>,
    orig_path: Option<Vec<u8>>,
    line: Option<Vec<u8>>,
    csv: Option<Value>,
    line_num: i64,
    max_line_len: i64,
    delimiter: u8,
    enclosure: u8,
    escape: Option<u8>,
    escape_set: bool,
}

/// `spl_filesystem_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// `spl_filesystem_object_check_handlers`, `SplFileObject`'s and
/// `SplTempFileObject`'s: the same slots with `clone_obj` null, so the
/// two are uncloneable as php's are.
pub(crate) struct FileHandlers;

pub(crate) static FILE_HANDLERS: FileHandlers = FileHandlers;

/// The table a class of this file is reached through.
///
/// ```php
/// $i = clone new SplFileInfo(__FILE__);
/// echo get_class($i), "\n";
/// try { clone new SplTempFileObject(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// SplFileInfo
/// Trying to clone an uncloneable object of class SplTempFileObject
/// ```
pub(crate) fn handlers(decl: &php_ext::decl::ClassDecl) -> &'static dyn ObjectHandlers {
    match decl.name.0.as_ref() {
        b"SplFileObject" | b"SplTempFileObject" => &FILE_HANDLERS,
        _ => &HANDLERS,
    }
}

const STUB: &str = "
class SplFileInfo implements Stringable {
    public function __construct(string $filename) {}
    /** @tentative-return-type */ public function getPath(): string {}
    /** @tentative-return-type */ public function getFilename(): string {}
    /** @tentative-return-type */ public function getExtension(): string {}
    /** @tentative-return-type */ public function getBasename(string $suffix = \"\"): string {}
    /** @tentative-return-type */ public function getPathname(): string {}
    /** @tentative-return-type */ public function getPerms(): int|false {}
    /** @tentative-return-type */ public function getInode(): int|false {}
    /** @tentative-return-type */ public function getSize(): int|false {}
    /** @tentative-return-type */ public function getOwner(): int|false {}
    /** @tentative-return-type */ public function getGroup(): int|false {}
    /** @tentative-return-type */ public function getATime(): int|false {}
    /** @tentative-return-type */ public function getMTime(): int|false {}
    /** @tentative-return-type */ public function getCTime(): int|false {}
    /** @tentative-return-type */ public function getType(): string|false {}
    /** @tentative-return-type */ public function isWritable(): bool {}
    /** @tentative-return-type */ public function isReadable(): bool {}
    /** @tentative-return-type */ public function isExecutable(): bool {}
    /** @tentative-return-type */ public function isFile(): bool {}
    /** @tentative-return-type */ public function isDir(): bool {}
    /** @tentative-return-type */ public function isLink(): bool {}
    /** @tentative-return-type */ public function getLinkTarget(): string|false {}
    /** @tentative-return-type */ public function getRealPath(): string|false {}
    /** @tentative-return-type */ public function getFileInfo(?string $class = null): SplFileInfo {}
    /** @tentative-return-type */ public function getPathInfo(?string $class = null): ?SplFileInfo {}
    /** @tentative-return-type */ public function openFile(string $mode = \"r\", bool $useIncludePath = false, $context = null): SplFileObject {}
    /** @tentative-return-type */ public function setFileClass(string $class = SplFileObject::class): void {}
    /** @tentative-return-type */ public function setInfoClass(string $class = SplFileInfo::class): void {}
    public function __toString(): string {}
    /** @tentative-return-type */ public function __debugInfo(): array {}
    #[\\Deprecated(since: '8.2')]
    final public function _bad_state_ex(): void {}
}
class DirectoryIterator extends SplFileInfo implements SeekableIterator {
    public function __construct(string $directory) {}
    /** @tentative-return-type */ public function getFilename(): string {}
    /** @tentative-return-type */ public function getExtension(): string {}
    /** @tentative-return-type */ public function getBasename(string $suffix = \"\"): string {}
    /** @tentative-return-type */ public function isDot(): bool {}
    /** @tentative-return-type */ public function rewind(): void {}
    /** @tentative-return-type */ public function valid(): bool {}
    /** @tentative-return-type */ public function key(): mixed {}
    /** @tentative-return-type */ public function current(): mixed {}
    /** @tentative-return-type */ public function next(): void {}
    /** @tentative-return-type */ public function seek(int $offset): void {}
    public function __toString(): string {}
}
class FilesystemIterator extends DirectoryIterator {
    public const int CURRENT_MODE_MASK = 240;
    public const int CURRENT_AS_PATHNAME = 32;
    public const int CURRENT_AS_FILEINFO = 0;
    public const int CURRENT_AS_SELF = 16;
    public const int KEY_MODE_MASK = 3840;
    public const int KEY_AS_PATHNAME = 0;
    public const int FOLLOW_SYMLINKS = 16384;
    public const int KEY_AS_FILENAME = 256;
    public const int NEW_CURRENT_AND_KEY = 256;
    public const int OTHER_MODE_MASK = 28672;
    public const int SKIP_DOTS = 4096;
    public const int UNIX_PATHS = 8192;
    public function __construct(string $directory, int $flags = FilesystemIterator::KEY_AS_PATHNAME | FilesystemIterator::CURRENT_AS_FILEINFO | FilesystemIterator::SKIP_DOTS) {}
    /** @tentative-return-type */ public function rewind(): void {}
    /** @tentative-return-type */ public function key(): string {}
    /** @tentative-return-type */ public function current(): string|SplFileInfo|FilesystemIterator {}
    /** @tentative-return-type */ public function getFlags(): int {}
    /** @tentative-return-type */ public function setFlags(int $flags): void {}
}
class RecursiveDirectoryIterator extends FilesystemIterator implements RecursiveIterator {
    public function __construct(string $directory, int $flags = FilesystemIterator::KEY_AS_PATHNAME | FilesystemIterator::CURRENT_AS_FILEINFO) {}
    /** @tentative-return-type */ public function hasChildren(bool $allowLinks = false): bool {}
    /** @tentative-return-type */ public function getChildren(): RecursiveDirectoryIterator {}
    /** @tentative-return-type */ public function getSubPath(): string {}
    /** @tentative-return-type */ public function getSubPathname(): string {}
}
class GlobIterator extends FilesystemIterator implements Countable {
    public function __construct(string $pattern, int $flags = FilesystemIterator::KEY_AS_PATHNAME | FilesystemIterator::CURRENT_AS_FILEINFO) {}
    /** @tentative-return-type */ public function count(): int {}
}
class SplFileObject extends SplFileInfo implements RecursiveIterator, SeekableIterator {
    public const int DROP_NEW_LINE = 1;
    public const int READ_AHEAD = 2;
    public const int SKIP_EMPTY = 4;
    public const int READ_CSV = 8;
    public function __construct(string $filename, string $mode = \"r\", bool $useIncludePath = false, $context = null) {}
    /** @tentative-return-type */ public function rewind(): void {}
    /** @tentative-return-type */ public function eof(): bool {}
    /** @tentative-return-type */ public function valid(): bool {}
    /** @tentative-return-type */ public function fgets(): string {}
    /** @tentative-return-type */ public function fread(int $length): string|false {}
    /** @tentative-return-type */ public function fgetcsv(string $separator = \",\", string $enclosure = \"\\\"\", string $escape = \"\\\\\"): array|false {}
    /** @tentative-return-type */ public function fputcsv(array $fields, string $separator = \",\", string $enclosure = \"\\\"\", string $escape = \"\\\\\", string $eol = \"\\n\"): int|false {}
    /** @tentative-return-type */ public function setCsvControl(string $separator = \",\", string $enclosure = \"\\\"\", string $escape = \"\\\\\"): void {}
    /** @tentative-return-type */ public function getCsvControl(): array {}
    /** @tentative-return-type */ public function flock(int $operation, &$wouldBlock = null): bool {}
    /** @tentative-return-type */ public function fflush(): bool {}
    /** @tentative-return-type */ public function ftell(): int|false {}
    /** @tentative-return-type */ public function fseek(int $offset, int $whence = SEEK_SET): int {}
    /** @tentative-return-type */ public function fgetc(): string|false {}
    /** @tentative-return-type */ public function fpassthru(): int {}
    /** @tentative-return-type */ public function fscanf(string $format, mixed &...$vars): array|int|null {}
    /** @tentative-return-type */ public function fwrite(string $data, ?int $length = null): int|false {}
    /** @tentative-return-type */ public function fstat(): array {}
    /** @tentative-return-type */ public function ftruncate(int $size): bool {}
    /** @tentative-return-type */ public function current(): string|array|false {}
    /** @tentative-return-type */ public function key(): int {}
    /** @tentative-return-type */ public function next(): void {}
    /** @tentative-return-type */ public function setFlags(int $flags): void {}
    /** @tentative-return-type */ public function getFlags(): int {}
    /** @tentative-return-type */ public function setMaxLineLen(int $maxLength): void {}
    /** @tentative-return-type */ public function getMaxLineLen(): int {}
    /** @tentative-return-type */ public function hasChildren(): false {}
    /** @tentative-return-type */ public function getChildren(): null {}
    /** @tentative-return-type */ public function seek(int $line): void {}
    /** @tentative-return-type */ public function getCurrentLine(): string {}
    public function __toString(): string {}
}
class SplTempFileObject extends SplFileObject {
    public function __construct(int $maxMemory = 2 * 1024 * 1024) {}
}
";

/// The seven classes in `PHP_MINIT_FUNCTION(spl_directory)`'s order,
/// each method with its entry; `SplFileObject::getCurrentLine` is php's
/// alias of `fgets`.
///
/// ```php
/// $d = __DIR__ . '/spl-dir'; @mkdir($d); touch("$d/b.txt"); touch("$d/a.php");
/// $names = []; foreach (new DirectoryIterator($d) as $k => $f) { if (!$f->isDot()) $names[$k] = $f->getFilename(); }
/// sort($names); echo implode(',', $names), "\n";
/// $info = new SplFileInfo("$d/a.php");
/// var_dump($info->getExtension(), $info->getBasename('.php'), $info->isFile(), $info->getSize(), (string) $info === "$d/a.php");
/// $o = new SplFileObject("$d/b.txt", 'w+'); $o->fwrite("one\ntwo\n"); $o->rewind();
/// foreach ($o as $n => $line) echo $n, ':', json_encode($line), ';';
/// echo "\n";
/// ```
/// ```expect
/// a.php,b.txt
/// string(3) "php"
/// string(1) "a"
/// bool(true)
/// int(0)
/// bool(true)
/// 0:"one\n";1:"two\n";2:"";
/// ```
// One row per method of the stub, which is what the linter counts.
#[allow(clippy::too_many_lines)]
pub(crate) fn classes() -> Vec<Declared> {
    let rows: Vec<(&str, Entry)> = vec![
        ("SplFileInfo::__construct", info_construct),
        ("SplFileInfo::getPath", get_path),
        ("SplFileInfo::getFilename", get_filename),
        ("SplFileInfo::getExtension", get_extension),
        ("SplFileInfo::getBasename", get_basename),
        ("SplFileInfo::getPathname", get_pathname),
        ("SplFileInfo::getPerms", get_perms),
        ("SplFileInfo::getInode", get_inode),
        ("SplFileInfo::getSize", get_size),
        ("SplFileInfo::getOwner", get_owner),
        ("SplFileInfo::getGroup", get_group),
        ("SplFileInfo::getATime", get_atime),
        ("SplFileInfo::getMTime", get_mtime),
        ("SplFileInfo::getCTime", get_ctime),
        ("SplFileInfo::getType", get_type),
        ("SplFileInfo::isWritable", is_writable),
        ("SplFileInfo::isReadable", is_readable),
        ("SplFileInfo::isExecutable", is_executable),
        ("SplFileInfo::isFile", is_file),
        ("SplFileInfo::isDir", is_dir),
        ("SplFileInfo::isLink", is_link),
        ("SplFileInfo::getLinkTarget", get_link_target),
        ("SplFileInfo::getRealPath", get_real_path),
        ("SplFileInfo::getFileInfo", get_file_info),
        ("SplFileInfo::getPathInfo", get_path_info),
        ("SplFileInfo::openFile", open_file),
        ("SplFileInfo::setFileClass", set_file_class),
        ("SplFileInfo::setInfoClass", set_info_class),
        ("SplFileInfo::__toString", get_pathname),
        ("SplFileInfo::__debugInfo", debug_info),
        ("SplFileInfo::_bad_state_ex", bad_state),
        ("DirectoryIterator::__construct", dir_construct),
        ("DirectoryIterator::getFilename", dir_get_filename),
        ("DirectoryIterator::getExtension", dir_get_extension),
        ("DirectoryIterator::getBasename", dir_get_basename),
        ("DirectoryIterator::isDot", dir_is_dot),
        ("DirectoryIterator::rewind", dir_rewind),
        ("DirectoryIterator::valid", dir_valid),
        ("DirectoryIterator::key", dir_key),
        ("DirectoryIterator::current", dir_current),
        ("DirectoryIterator::next", dir_next),
        ("DirectoryIterator::seek", dir_seek),
        ("DirectoryIterator::__toString", dir_get_filename),
        ("FilesystemIterator::__construct", fs_construct),
        ("FilesystemIterator::rewind", dir_rewind),
        ("FilesystemIterator::key", fs_key),
        ("FilesystemIterator::current", fs_current),
        ("FilesystemIterator::getFlags", fs_get_flags),
        ("FilesystemIterator::setFlags", fs_set_flags),
        (
            "RecursiveDirectoryIterator::__construct",
            recursive_construct,
        ),
        (
            "RecursiveDirectoryIterator::hasChildren",
            recursive_has_children,
        ),
        (
            "RecursiveDirectoryIterator::getChildren",
            recursive_get_children,
        ),
        (
            "RecursiveDirectoryIterator::getSubPath",
            recursive_get_sub_path,
        ),
        (
            "RecursiveDirectoryIterator::getSubPathname",
            recursive_get_sub_pathname,
        ),
        ("GlobIterator::__construct", glob_construct),
        ("GlobIterator::count", glob_count),
        ("SplFileObject::__construct", file_construct),
        ("SplFileObject::rewind", file_rewind),
        ("SplFileObject::eof", file_eof),
        ("SplFileObject::valid", file_valid),
        ("SplFileObject::fgets", file_fgets),
        ("SplFileObject::fread", file_fread),
        ("SplFileObject::fgetcsv", file_fgetcsv),
        ("SplFileObject::fputcsv", file_fputcsv),
        ("SplFileObject::setCsvControl", file_set_csv_control),
        ("SplFileObject::getCsvControl", file_get_csv_control),
        ("SplFileObject::flock", file_flock),
        ("SplFileObject::fflush", file_fflush),
        ("SplFileObject::ftell", file_ftell),
        ("SplFileObject::fseek", file_fseek),
        ("SplFileObject::fgetc", file_fgetc),
        ("SplFileObject::fpassthru", file_fpassthru),
        ("SplFileObject::fscanf", file_fscanf),
        ("SplFileObject::fwrite", file_fwrite),
        ("SplFileObject::fstat", file_fstat),
        ("SplFileObject::ftruncate", file_ftruncate),
        ("SplFileObject::current", file_current),
        ("SplFileObject::key", file_key),
        ("SplFileObject::next", file_next),
        ("SplFileObject::setFlags", file_set_flags),
        ("SplFileObject::getFlags", file_get_flags),
        ("SplFileObject::setMaxLineLen", file_set_max_line_len),
        ("SplFileObject::getMaxLineLen", file_get_max_line_len),
        ("SplFileObject::hasChildren", file_has_children),
        ("SplFileObject::getChildren", file_get_children),
        ("SplFileObject::seek", file_seek),
        ("SplFileObject::getCurrentLine", file_fgets),
        ("SplFileObject::__toString", file_to_string),
        ("SplTempFileObject::__construct", temp_construct),
    ];
    stub::classes(STUB, &rows)
}

fn fs<R>(
    req: &mut Request,
    object: PhpObject,
    change: impl FnOnce(&mut Fs) -> R,
) -> Result<R, Thrown> {
    if let Some(it) = object.native_mut::<Fs>(req) {
        return Ok(change(it));
    }
    Err(php_ext::raising::fatal(
        req,
        "InternalError: an SPL file method served on an object without its payload",
    ))
}

fn uninitialized(req: &mut Request) -> Thrown {
    throw(req, "Error", "Object not initialized")
}

/// `IS_SLASH`.
fn slash(b: u8) -> bool {
    b == b'/'
}

/// `spl_filesystem_info_set_filename`: the name without its trailing
/// slashes, and the path up to the last slash before it.
fn set_filename(req: &mut Request, this: PhpObject, path: &[u8]) -> Result<(), Thrown> {
    let mut len = path.len();
    while len > 1 && slash(path[len - 1]) {
        len -= 1;
    }
    let file_name = path[..len].to_vec();
    let mut at = len;
    while at > 1 && !slash(path[at - 1]) {
        at -= 1;
    }
    at = at.saturating_sub(1);
    let dir = path[..at].to_vec();
    fs(req, this, |it| {
        it.file_name = Some(file_name);
        it.path = Some(dir);
    })
}

/// `spl_filesystem_object_get_path`: the path, a glob's directory for a
/// `GlobIterator`.
fn object_path(req: &mut Request, this: PhpObject) -> Result<Option<Vec<u8>>, Thrown> {
    fs(req, this, |it| {
        if it.kind == Type::Dir
            && let Some((_, entries)) = &it.glob
        {
            let current = usize::try_from(it.index).ok().and_then(|i| entries.get(i));
            return current.map(|e| dirname(e));
        }
        it.path.clone()
    })
}

/// `zend_dirname` enough for a path this file builds: everything
/// before the last slash, `.` for a name with none.
fn dirname(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|b| slash(*b)) {
        Some(0) => b"/".to_vec(),
        Some(at) => path[..at].to_vec(),
        None => b".".to_vec(),
    }
}

/// `spl_filesystem_object_get_file_name`: the known name, or for a
/// directory the path joined with the entry.
fn file_name(req: &mut Request, this: PhpObject) -> Result<Vec<u8>, Thrown> {
    if let Some(name) = fs(req, this, |it| it.file_name.clone())? {
        return Ok(name);
    }
    let (kind, entry, flags) = fs(req, this, |it| (it.kind, it.entry.clone(), it.flags))?;
    if kind != Type::Dir {
        return Err(uninitialized(req));
    }
    let glob = fs(req, this, |it| it.glob.is_some())?;
    if glob {
        let name = fs(req, this, |it| {
            it.glob.as_ref().and_then(|(_, e)| {
                usize::try_from(it.index)
                    .ok()
                    .and_then(|i| e.get(i).cloned())
            })
        })?
        .unwrap_or_default();
        fs(req, this, |it| it.file_name = Some(name.clone()))?;
        return Ok(name);
    }
    let path = object_path(req, this)?;
    let name = match path {
        Some(path) if !path.is_empty() => {
            let _ = flags & UNIX_PATHS;
            [path.as_slice(), b"/", entry.as_slice()].concat()
        }
        _ => entry,
    };
    fs(req, this, |it| it.file_name = Some(name.clone()))?;
    Ok(name)
}

/// `spl_filesystem_object_get_pathname`.
fn pathname(req: &mut Request, this: PhpObject) -> Result<Option<Vec<u8>>, Thrown> {
    let (kind, entry) = fs(req, this, |it| (it.kind, it.entry.is_empty()))?;
    match kind {
        Type::Info | Type::File => fs(req, this, |it| it.file_name.clone()),
        Type::Dir if !entry => file_name(req, this).map(Some),
        Type::Dir => Ok(None),
    }
}

/// The file name after the path, what `getFilename()` answers.
fn short_name(req: &mut Request, this: PhpObject) -> Result<Vec<u8>, Thrown> {
    let Some(name) = fs(req, this, |it| it.file_name.clone())? else {
        return Err(uninitialized(req));
    };
    let path = object_path(req, this)?.unwrap_or_default();
    if !path.is_empty() && path.len() < name.len() {
        return Ok(name[path.len() + 1..].to_vec());
    }
    Ok(name)
}

/// `php_basename`, with the suffix cut where it is a proper tail.
fn basename(name: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut end = name.len();
    while end > 0 && slash(name[end - 1]) {
        end -= 1;
    }
    let start = name[..end]
        .iter()
        .rposition(|b| slash(*b))
        .map_or(0, |p| p + 1);
    let base = &name[start..end];
    if !suffix.is_empty() && base.len() > suffix.len() && base.ends_with(suffix) {
        return base[..base.len() - suffix.len()].to_vec();
    }
    base.to_vec()
}

fn extension(name: &[u8]) -> Vec<u8> {
    let base = basename(name, b"");
    base.iter()
        .rposition(|b| *b == b'.')
        .map_or_else(Vec::new, |at| base[at + 1..].to_vec())
}

impl ObjectHandlers for Handlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let held = object
            .native_mut::<Fs>(req)
            .map(|it| (it.dir.take(), it.stream.take(), it.csv.take()));
        if let Some((dir, stream, csv)) = held {
            if let Some(dir) = dir {
                dir.close(req);
            }
            if let Some(stream) = stream {
                stream.close(req);
            }
            if let Some(csv) = csv {
                req.release(csv);
            }
        }
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_fs)
    }
}

impl ObjectHandlers for FileHandlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        HANDLERS.free_obj(req, object);
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }
}

/// `spl_filesystem_object_clone`: an info copied; a directory opened
/// again and read to where it stood; a file refused, as php's
/// `ZEND_UNREACHABLE` guards.
fn clone_fs(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let (kind, path, name, flags, index, file_class, info_class) = fs(req, object, |it| {
        (
            it.kind,
            it.path.clone(),
            it.file_name.clone(),
            it.flags,
            it.index,
            it.file_class,
            it.info_class,
        )
    })?;
    if kind == Type::File {
        return Err(throw(
            req,
            "Error",
            "Trying to clone an uncloneable object of class SplFileObject",
        ));
    }
    let copy = Fs {
        kind: Type::Info,
        path: path.clone(),
        file_name: name,
        flags,
        file_class,
        info_class,
        ..Fs::default()
    };
    let cloned = php_ext::call::clone_with(req, &object, copy)?;
    if kind == Type::Dir
        && let Some(path) = path
    {
        dir_open(req, cloned, &path)?;
        for _ in 0..index {
            dir_step(req, cloned)?;
        }
        fs(req, cloned, |it| it.index = index)?;
    }
    Ok(cloned)
}

/// `SplFileInfo::__construct(string $filename)`.
///
/// ```php
/// $i = new SplFileInfo('/a/b/c.tar.gz/');
/// var_dump($i->getPath(), $i->getFilename(), $i->getPathname(), $i->getExtension(), $i->getBasename('.gz'));
/// $j = new SplFileInfo('plain');
/// var_dump($j->getPath(), $j->getFilename(), (string) $j);
/// try { new SplFileInfo("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "/a/b"
/// string(8) "c.tar.gz"
/// string(13) "/a/b/c.tar.gz"
/// string(2) "gz"
/// string(5) "c.tar"
/// string(0) ""
/// string(5) "plain"
/// string(5) "plain"
/// SplFileInfo::__construct(): Argument #1 ($filename) must not contain any null bytes
/// ```
fn info_construct(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let path = path_argument(req, 1)?;
    set_filename(req, this, &path)?;
    Ok(Value::null())
}

/// `Z_PARAM_PATH`: a string with no NUL byte.
fn path_argument(req: &mut Request, position: u32) -> Result<Vec<u8>, Thrown> {
    let path = crate::string(req, position)?.as_bytes().to_vec();
    if path.contains(&0) {
        return Err(arg_value_error(req, position, "not contain any null bytes"));
    }
    Ok(path)
}

/// `getPath(): string`.
fn get_path(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let path = object_path(req, this)?.unwrap_or_default();
    crate::string_value(req, &path)
}

/// `getFilename(): string`.
fn get_filename(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let name = short_name(req, this)?;
    crate::string_value(req, &name)
}

/// `getExtension(): string`.
fn get_extension(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let name = short_name(req, this)?;
    crate::string_value(req, &extension(&name))
}

/// `getBasename(string $suffix = ""): string`.
fn get_basename(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    let this = crate::this(req)?;
    let suffix = match crate::given(req, 1) {
        Some(_) => crate::string(req, 1)?.as_bytes().to_vec(),
        None => Vec::new(),
    };
    let name = short_name(req, this)?;
    crate::string_value(req, &basename(&name, &suffix))
}

/// `getPathname(): string` and `__toString(): string`.
fn get_pathname(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let name = pathname(req, this)?.unwrap_or_default();
    crate::string_value(req, &name)
}

/// `php_stat`'s stat of the object's file, and php's
/// `RuntimeException` naming the method where it fails.
/// A file's name beside its stat, `php_fstat`'s thirteen fields.
type Stat = (Vec<u8>, [i64; 13]);

fn stat(req: &mut Request, link: bool) -> Result<Option<Stat>, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let name = file_name(req, this)?;
    let got = req.url_stat(&name, link, true)?;
    Ok(got.map(|s| (name, s)))
}

fn stat_field(req: &mut Request, field: usize, what: &str) -> Result<Value, Thrown> {
    let this = crate::this(req)?;
    if let Some((_, s)) = stat(req, false)? {
        return Ok(Value::int(s[field]));
    }
    let name = String::from_utf8_lossy(&file_name(req, this)?).into_owned();
    Err(throw(
        req,
        "RuntimeException",
        &format!("SplFileInfo::{what}(): stat failed for {name}"),
    ))
}

/// `getPerms(): int|false`.
///
/// ```php
/// $f = __DIR__ . '/spl-stat.txt'; file_put_contents($f, 'abc');
/// $i = new SplFileInfo($f);
/// var_dump($i->getSize(), $i->getType(), $i->isDir(), $i->isLink(), $i->isReadable(), $i->getMTime() === filemtime($f), $i->getPerms() === fileperms($f), $i->getInode() === fileinode($f));
/// try { (new SplFileInfo(__DIR__ . '/missing'))->getSize(); } catch (RuntimeException $e) { echo str_replace(__DIR__, '', $e->getMessage()), "\n"; }
/// var_dump((new SplFileInfo(__DIR__ . '/missing'))->isFile());
/// ```
/// ```expect
/// int(3)
/// string(4) "file"
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// SplFileInfo::getSize(): stat failed for /missing
/// bool(false)
/// ```
fn get_perms(req: &mut Request) -> Result<Value, Thrown> {
    stat_field(req, 2, "getPerms")
}

/// `getInode(): int|false`; the witness is `getPerms`'.
fn get_inode(req: &mut Request) -> Result<Value, Thrown> {
    stat_field(req, 1, "getInode")
}

/// `getSize(): int|false`.
fn get_size(req: &mut Request) -> Result<Value, Thrown> {
    stat_field(req, 7, "getSize")
}

/// `getOwner(): int|false`.
fn get_owner(req: &mut Request) -> Result<Value, Thrown> {
    stat_field(req, 4, "getOwner")
}

/// `getGroup(): int|false`.
fn get_group(req: &mut Request) -> Result<Value, Thrown> {
    stat_field(req, 5, "getGroup")
}

/// `getATime(): int|false`.
fn get_atime(req: &mut Request) -> Result<Value, Thrown> {
    stat_field(req, 8, "getATime")
}

/// `getMTime(): int|false`.
fn get_mtime(req: &mut Request) -> Result<Value, Thrown> {
    stat_field(req, 9, "getMTime")
}

/// `getCTime(): int|false`.
fn get_ctime(req: &mut Request) -> Result<Value, Thrown> {
    stat_field(req, 10, "getCTime")
}

/// `S_IFMT`'s kinds, `php_stat`'s `FS_TYPE` names.
fn kind_name(mode: i64) -> &'static [u8] {
    match mode & 0o170_000 {
        0o010_000 => b"fifo",
        0o020_000 => b"char",
        0o040_000 => b"dir",
        0o060_000 => b"block",
        0o100_000 => b"file",
        0o120_000 => b"link",
        0o140_000 => b"socket",
        _ => b"unknown",
    }
}

/// `getType(): string|false`, of the link itself for a link.
fn get_type(req: &mut Request) -> Result<Value, Thrown> {
    let this = crate::this(req)?;
    if let Some((_, s)) = stat(req, true)? {
        return crate::string_value(req, kind_name(s[2]));
    }
    let name = String::from_utf8_lossy(&file_name(req, this)?).into_owned();
    Err(throw(
        req,
        "RuntimeException",
        &format!("SplFileInfo::getType(): Lstat failed for {name}"),
    ))
}

/// The `st_mode` test of `FS_IS_FILE`, `FS_IS_DIR` and `FS_IS_LINK`: `false`
/// for a stat that fails, which these three raise nothing for.
fn is_kind(req: &mut Request, link: bool, kind: i64) -> Result<Value, Thrown> {
    Ok(Value::bool(
        stat(req, link)?.is_some_and(|(_, s)| s[2] & 0o170_000 == kind),
    ))
}

/// `isFile(): bool`.
fn is_file(req: &mut Request) -> Result<Value, Thrown> {
    is_kind(req, false, 0o100_000)
}

/// `isDir(): bool`.
fn is_dir(req: &mut Request) -> Result<Value, Thrown> {
    is_kind(req, false, 0o040_000)
}

/// `isLink(): bool`.
fn is_link(req: &mut Request) -> Result<Value, Thrown> {
    is_kind(req, true, 0o120_000)
}

/// The access test through `ext/standard`'s native of the same name,
/// php's `FS_IS_W`, `FS_IS_R` and `FS_IS_X`.
fn access(req: &mut Request, native: &[u8]) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let name = file_name(req, this)?;
    let name = crate::string_value(req, &name)?;
    let answered = crate::standard(req, native, &[name]);
    req.release(name);
    answered
}

/// `isWritable(): bool`.
fn is_writable(req: &mut Request) -> Result<Value, Thrown> {
    access(req, b"is_writable")
}

/// `isReadable(): bool`.
fn is_readable(req: &mut Request) -> Result<Value, Thrown> {
    access(req, b"is_readable")
}

/// `isExecutable(): bool`.
fn is_executable(req: &mut Request) -> Result<Value, Thrown> {
    access(req, b"is_executable")
}

/// `getLinkTarget(): string|false`: the link read, php's
/// `RuntimeException` for a path that is none.
///
/// ```php
/// $f = __DIR__ . '/spl-target.txt'; touch($f);
/// try { (new SplFileInfo($f))->getLinkTarget(); } catch (RuntimeException $e) { echo str_replace(__DIR__, '', $e->getMessage()), "\n"; }
/// var_dump(str_replace(__DIR__, '', (new SplFileInfo($f))->getRealPath()), (new SplFileInfo(__DIR__ . '/none'))->getRealPath());
/// ```
/// ```expect
/// Unable to read link /spl-target.txt, error: Invalid argument
/// string(15) "/spl-target.txt"
/// bool(false)
/// ```
fn get_link_target(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let name = file_name(req, this)?;
    let host = req.absolutize(&name);
    match std::fs::read_link(&host.0) {
        Ok(target) => crate::string_value(req, target.as_os_str().as_encoded_bytes()),
        Err(error) => {
            let text = error.to_string();
            let text = text
                .split_once(" (os error")
                .map_or(text.as_str(), |(t, _)| t)
                .to_owned();
            let shown = String::from_utf8_lossy(&name).into_owned();
            Err(throw(
                req,
                "RuntimeException",
                &format!("Unable to read link {shown}, error: {text}"),
            ))
        }
    }
}

/// `getRealPath(): string|false`.
fn get_real_path(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let (kind, known, entry, orig) = fs(req, this, |it| {
        (
            it.kind,
            it.file_name.is_some(),
            !it.entry.is_empty(),
            it.orig_path.clone(),
        )
    })?;
    if kind == Type::Dir && !known && entry {
        file_name(req, this)?;
    }
    let name = match orig {
        Some(orig) => Some(orig),
        None => fs(req, this, |it| it.file_name.clone())?,
    };
    match name.and_then(|n| req.realpath(&n)) {
        Some(real) => crate::string_value(req, real.0.as_os_str().as_encoded_bytes()),
        None => Ok(Value::bool(false)),
    }
}

/// `Z_PARAM_CLASS` of a class derived from `base` at `position`, and
/// `Z_PARAM_CLASS_OR_NULL` where `nullable`: php's `TypeError` names
/// the base as the parameter's default names it.
fn class_argument(
    req: &mut Request,
    position: u32,
    base: &[u8],
    nullable: bool,
) -> Result<Option<ClassId>, Thrown> {
    let given = crate::value(req, position);
    if matches!(given.as_ref(), ValueRef::Null | ValueRef::Undef) {
        return Ok(None);
    }
    let name = crate::string(req, position)?;
    let found = lookup_class(req, name.as_bytes())?;
    match found {
        Some(class) if crate::is_a(req, class, base) => Ok(Some(class)),
        _ => {
            let shown = String::from_utf8_lossy(name.as_bytes()).into_owned();
            let base = String::from_utf8_lossy(base).into_owned();
            let null = if nullable { " or null" } else { "" };
            Err(arg_type_error(
                req,
                position,
                &format!("be a class name derived from {base}{null}, {shown} given"),
            ))
        }
    }
}

/// The name of the info class an object makes, `SplFileInfo` by default.
fn info_base(req: &mut Request, this: PhpObject) -> Result<Vec<u8>, Thrown> {
    Ok(match fs(req, this, |it| it.info_class)? {
        Some(class) => crate::class_name(req, class),
        None => b"SplFileInfo".to_vec(),
    })
}

fn class_or(
    req: &mut Request,
    name: &[u8],
    chosen: Option<ClassId>,
) -> Result<Option<ClassId>, Thrown> {
    match chosen {
        Some(class) => Ok(Some(class)),
        None => lookup_class(req, name),
    }
}

/// `spl_filesystem_object_create_info`: an object of `class` over
/// `path`, its constructor served where a user class writes one.
fn create_info(req: &mut Request, path: &[u8], class: ClassId) -> Result<Value, Thrown> {
    let object = new_object(req, class)?;
    if crate::overridden(req, object, b"__construct") {
        let arg = crate::string_value(req, path)?;
        let served = call_method(req, &object, b"__construct", &[arg]);
        req.release(arg);
        match served {
            Ok(answered) => req.release(answered),
            Err(thrown) => {
                req.release(object.as_value());
                return Err(thrown);
            }
        }
    } else {
        set_filename(req, object, path)?;
    }
    Ok(object.as_value())
}

/// `getFileInfo(?string $class = null): SplFileInfo`.
///
/// ```php
/// $i = new SplFileInfo('/x/y.txt');
/// var_dump(get_class($i->getFileInfo()), $i->getFileInfo()->getFilename(), $i->getPathInfo()->getPathname());
/// class Info extends SplFileInfo {}
/// $i->setInfoClass('Info'); var_dump(get_class($i->getPathInfo()));
/// try { $i->getFileInfo('stdClass'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(11) "SplFileInfo"
/// string(5) "y.txt"
/// string(2) "/x"
/// string(4) "Info"
/// SplFileInfo::getFileInfo(): Argument #1 ($class) must be a class name derived from Info or null, stdClass given
/// ```
fn get_file_info(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    let this = crate::this(req)?;
    let base = info_base(req, this)?;
    let chosen = class_argument(req, 1, &base, true)?;
    let chosen = match chosen {
        Some(c) => Some(c),
        None => fs(req, this, |it| it.info_class)?,
    };
    let Some(class) = class_or(req, b"SplFileInfo", chosen)? else {
        return Ok(Value::null());
    };
    let (kind, entry) = fs(req, this, |it| (it.kind, it.entry.is_empty()))?;
    if kind == Type::Dir && entry {
        return Err(throw(req, "RuntimeException", "Could not open file"));
    }
    let name = file_name(req, this)?;
    create_info(req, &name, class)
}

/// `getPathInfo(?string $class = null): ?SplFileInfo`; the witness is
/// `getFileInfo`'s.
fn get_path_info(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    let this = crate::this(req)?;
    let base = b"SplFileInfo".to_vec();
    let chosen = class_argument(req, 1, &base, true)?;
    let chosen = match chosen {
        Some(c) => Some(c),
        None => fs(req, this, |it| it.info_class)?,
    };
    let Some(class) = class_or(req, b"SplFileInfo", chosen)? else {
        return Ok(Value::null());
    };
    match pathname(req, this)? {
        Some(path) if !path.is_empty() => create_info(req, &dirname(&path), class),
        _ => Ok(Value::null()),
    }
}

/// `openFile(string $mode = "r", bool $useIncludePath = false, $context
/// = null): SplFileObject`, an object of the file class over the file.
///
/// ```php
/// $f = __DIR__ . '/spl-open.txt'; file_put_contents($f, "x\n");
/// $o = (new SplFileInfo($f))->openFile();
/// var_dump(get_class($o), $o->fgets());
/// try { (new SplFileInfo(__DIR__))->openFile(); } catch (LogicException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(13) "SplFileObject"
/// string(2) "x
/// "
/// Cannot use SplFileObject with directories
/// ```
fn open_file(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 3)?;
    let this = crate::this(req)?;
    let mode = match crate::given(req, 1) {
        Some(_) => crate::string(req, 1)?.as_bytes().to_vec(),
        None => b"r".to_vec(),
    };
    let include = crate::bool_or(req, 2, false)?;
    let chosen = fs(req, this, |it| it.file_class)?;
    let Some(class) = class_or(req, b"SplFileObject", chosen)? else {
        return Ok(Value::null());
    };
    let (kind, entry) = fs(req, this, |it| (it.kind, it.entry.is_empty()))?;
    if kind == Type::Dir && entry {
        return Err(throw(req, "RuntimeException", "Could not open file"));
    }
    let name = file_name(req, this)?;
    let name_value = crate::string_value(req, &name)?;
    let mode_value = crate::string_value(req, &mode)?;
    let opened = instantiate(req, class, &[name_value, mode_value, Value::bool(include)]);
    req.release(name_value);
    req.release(mode_value);
    Ok(opened?.as_value())
}

/// `setFileClass(string $class = SplFileObject::class): void`.
fn set_file_class(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    let this = crate::this(req)?;
    let class = class_argument(req, 1, b"SplFileObject", false)?;
    fs(req, this, |it| it.file_class = class)?;
    Ok(Value::null())
}

/// `setInfoClass(string $class = SplFileInfo::class): void`.
fn set_info_class(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    let this = crate::this(req)?;
    let class = class_argument(req, 1, b"SplFileInfo", false)?;
    fs(req, this, |it| it.info_class = class)?;
    Ok(Value::null())
}

/// `__debugInfo(): array`: the properties, `pathName` and `fileName`
/// under `SplFileInfo`'s private names, `glob` and `subPathName` for a
/// directory, and `openMode`, `delimiter` and `enclosure` for a file.
///
/// ```php
/// var_dump(new SplFileInfo('/tmp/x.txt'));
/// ```
/// ```expect
/// object(SplFileInfo)#1 (2) {
///   ["pathName":"SplFileInfo":private]=>
///   string(10) "/tmp/x.txt"
///   ["fileName":"SplFileInfo":private]=>
///   string(5) "x.txt"
/// }
/// ```
fn debug_info(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let mut out = crate::members(req, this)?;
    let path_name = pathname(req, this)?.unwrap_or_default();
    let set = |req: &mut Request, out: &mut PhpArray, class: &[u8], field: &[u8], value: Value| {
        let name = [b"\0".as_slice(), class, b"\0", field].concat();
        let name = PhpString::new(req, &name)?;
        out.set(req, Key::Str(name), value)
    };
    let value = crate::string_value(req, &path_name)?;
    set(req, &mut out, b"SplFileInfo", b"pathName", value)?;
    if fs(req, this, |it| it.file_name.is_some())? {
        let name = short_name(req, this)?;
        let value = crate::string_value(req, &name)?;
        set(req, &mut out, b"SplFileInfo", b"fileName", value)?;
    }
    let (kind, glob, sub_path, mode, delimiter, enclosure, path) = fs(req, this, |it| {
        (
            it.kind,
            it.glob.as_ref().map(|(p, _)| p.clone()),
            it.sub_path.clone(),
            it.open_mode.clone(),
            it.delimiter,
            it.enclosure,
            it.path.clone(),
        )
    })?;
    if kind == Type::Dir {
        let value = match glob {
            Some(_) => crate::string_value(req, &path.unwrap_or_default())?,
            None => Value::bool(false),
        };
        set(req, &mut out, b"DirectoryIterator", b"glob", value)?;
        let value = crate::string_value(req, &sub_path.unwrap_or_default())?;
        set(
            req,
            &mut out,
            b"RecursiveDirectoryIterator",
            b"subPathName",
            value,
        )?;
    }
    if kind == Type::File {
        let value = crate::string_value(req, &mode)?;
        set(req, &mut out, b"SplFileObject", b"openMode", value)?;
        let value = crate::string_value(req, &[delimiter])?;
        set(req, &mut out, b"SplFileObject", b"delimiter", value)?;
        let value = crate::string_value(req, &[enclosure])?;
        set(req, &mut out, b"SplFileObject", b"enclosure", value)?;
    }
    Ok(Value::array(out))
}

/// `_bad_state_ex(): void`, deprecated: php's `Error`.
fn bad_state(req: &mut Request) -> Result<Value, Thrown> {
    php_ext::raising::deprecated(
        req,
        "Method SplFileInfo::_bad_state_ex() is deprecated since 8.2",
    );
    Err(throw(
        req,
        "Error",
        "The parent constructor was not called: the object is in an invalid state",
    ))
}

// ---- the directory iterators

/// `spl_filesystem_dir_read`: the next entry, empty past the last.
fn dir_read(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    let (dir, glob) = fs(req, this, |it| {
        it.file_name = None;
        (it.dir, it.glob.is_some())
    })?;
    if glob {
        let entry = fs(req, this, |it| {
            it.glob.as_ref().and_then(|(_, entries)| {
                usize::try_from(it.index)
                    .ok()
                    .and_then(|i| entries.get(i))
                    .map(|e| basename(e, b""))
            })
        })?;
        fs(req, this, |it| it.entry = entry.unwrap_or_default())?;
        return Ok(());
    }
    let entry = match dir {
        Some(dir) => dir.read_dir(req)?.map(|s| s.as_bytes().to_vec()),
        None => None,
    };
    fs(req, this, |it| it.entry = entry.unwrap_or_default())
}

fn is_dot(entry: &[u8]) -> bool {
    entry == b"." || entry == b".."
}

/// One step: the next entry, the dots stepped over under `SKIP_DOTS`.
fn dir_step(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    let skip = fs(req, this, |it| it.flags & SKIP_DOTS != 0)?;
    loop {
        dir_read(req, this)?;
        let entry = fs(req, this, |it| it.entry.clone())?;
        if !(skip && is_dot(&entry)) {
            return Ok(());
        }
        fs(req, this, |it| it.index += i64::from(it.glob.is_some()))?;
    }
}

/// `spl_filesystem_dir_open`: the directory opened, its path kept
/// without a trailing slash and its first entry read; php's
/// `UnexpectedValueException` where it will not open.
fn dir_open(req: &mut Request, this: PhpObject, path: &[u8]) -> Result<(), Thrown> {
    let trimmed = if path.len() > 1 && slash(path[path.len() - 1]) {
        &path[..path.len() - 1]
    } else {
        path
    };
    fs(req, this, |it| {
        it.kind = Type::Dir;
        it.path = Some(trimmed.to_vec());
        it.index = 0;
    })?;
    let opened = php_ext::raising::throwing_warnings(req, "UnexpectedValueException", |req| {
        req.open_dir(path)
    })?;
    let Some(dir) = opened else {
        let shown = String::from_utf8_lossy(path).into_owned();
        return Err(throw(
            req,
            "UnexpectedValueException",
            &format!("Failed to open directory \"{shown}\""),
        ));
    };
    fs(req, this, |it| it.dir = Some(dir))?;
    dir_step(req, this)
}

/// `spl_filesystem_object_construct`.
fn dir_construct_with(req: &mut Request, flags_given: bool, extra: i64) -> Result<(), Thrown> {
    crate::arity(req, 1, if flags_given { 2 } else { 1 })?;
    let this = crate::this(req)?;
    let path = path_argument(req, 1)?;
    let mut flags = extra;
    if flags_given {
        flags = crate::long_or(req, 2, extra | KEY_AS_PATHNAME | CURRENT_AS_FILEINFO)?;
    } else {
        flags |= KEY_AS_PATHNAME | CURRENT_AS_SELF;
    }
    if path.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if fs(req, this, |it| it.path.is_some())? {
        return Err(throw(
            req,
            "Error",
            "Directory object is already initialized",
        ));
    }
    fs(req, this, |it| it.flags = flags)?;
    dir_open(req, this, &path)
}

/// `DirectoryIterator::__construct(string $directory)`.
///
/// ```php
/// $d = __DIR__ . '/spl-dir2'; @mkdir($d); touch("$d/only.txt");
/// $it = new DirectoryIterator($d);
/// $seen = []; foreach ($it as $k => $entry) { $seen[] = ($entry === $it ? 'self ' : '') . ($entry->isDot() ? 'dot ' : '') . $entry->getFilename(); }
/// sort($seen); echo implode(', ', $seen), "\n";
/// $it->rewind(); $it->seek(2); var_dump($it->key(), $it->valid());
/// try { new DirectoryIterator(__DIR__ . '/nope'); } catch (UnexpectedValueException $e) { echo str_replace(__DIR__, '', $e->getMessage()), "\n"; }
/// try { new DirectoryIterator(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// self dot ., self dot .., self only.txt
/// int(2)
/// bool(true)
/// DirectoryIterator::__construct(/nope): Failed to open directory: No such file or directory
/// DirectoryIterator::__construct(): Argument #1 ($directory) must not be empty
/// ```
fn dir_construct(req: &mut Request) -> Result<Value, Thrown> {
    dir_construct_with(req, false, 0)?;
    Ok(Value::null())
}

/// `CHECK_DIRECTORY_ITERATOR_IS_INITIALIZED`.
fn this_dir(req: &mut Request) -> Result<PhpObject, Thrown> {
    let this = crate::this(req)?;
    if fs(req, this, |it| it.dir.is_none() && it.glob.is_none())? {
        return Err(uninitialized(req));
    }
    Ok(this)
}

/// `DirectoryIterator::getFilename(): string` and `__toString()`, the
/// entry's name.
fn dir_get_filename(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dir(req)?;
    let entry = fs(req, this, |it| it.entry.clone())?;
    crate::string_value(req, &entry)
}

/// `DirectoryIterator::getExtension(): string`.
fn dir_get_extension(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dir(req)?;
    let entry = fs(req, this, |it| it.entry.clone())?;
    crate::string_value(req, &extension(&entry))
}

/// `DirectoryIterator::getBasename(string $suffix = ""): string`.
fn dir_get_basename(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    let this = this_dir(req)?;
    let suffix = match crate::given(req, 1) {
        Some(_) => crate::string(req, 1)?.as_bytes().to_vec(),
        None => Vec::new(),
    };
    let entry = fs(req, this, |it| it.entry.clone())?;
    crate::string_value(req, &basename(&entry, &suffix))
}

/// `isDot(): bool`.
fn dir_is_dot(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dir(req)?;
    Ok(Value::bool(fs(req, this, |it| is_dot(&it.entry))?))
}

/// `rewind(): void`, `php_stream_rewinddir` and the first entry.
fn dir_rewind(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dir(req)?;
    let dir = fs(req, this, |it| {
        it.index = 0;
        it.dir
    })?;
    if let Some(dir) = dir {
        dir.seek(req, 0, Whence::Set);
    }
    dir_step(req, this)?;
    Ok(Value::null())
}

/// `valid(): bool`.
fn dir_valid(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dir(req)?;
    Ok(Value::bool(fs(req, this, |it| !it.entry.is_empty())?))
}

/// `DirectoryIterator::key(): mixed`, the index.
fn dir_key(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dir(req)?;
    Ok(Value::int(fs(req, this, |it| it.index)?))
}

/// `DirectoryIterator::current(): mixed`, the iterator itself.
fn dir_current(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dir(req)?;
    Ok(crate::copied(req, this.as_value()))
}

/// `next(): void`.
fn dir_next(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dir(req)?;
    fs(req, this, |it| it.index += 1)?;
    dir_step(req, this)?;
    Ok(Value::null())
}

/// `seek(int $offset): void`: `rewind()`, `valid()` and `next()` served
/// through the class, as php serves them.
fn dir_seek(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this_dir(req)?;
    let pos = crate::long(req, 1)?;
    if fs(req, this, |it| it.index)? > pos {
        crate::effect(req, this, b"rewind")?;
    }
    while fs(req, this, |it| it.index)? < pos {
        if !crate::valid(req, this)? {
            return Err(throw(
                req,
                "OutOfBoundsException",
                &format!("Seek position {pos} is out of range"),
            ));
        }
        crate::effect(req, this, b"next")?;
    }
    Ok(Value::null())
}

/// `FilesystemIterator::__construct(string $directory, int $flags =
/// KEY_AS_PATHNAME | CURRENT_AS_FILEINFO | SKIP_DOTS)`.
///
/// ```php
/// $d = __DIR__ . '/spl-fs'; @mkdir($d); touch("$d/f.txt");
/// $it = new FilesystemIterator($d);
/// foreach ($it as $k => $v) var_dump(str_replace($d, '', $k), get_class($v), $v->getFilename());
/// $it->setFlags(FilesystemIterator::KEY_AS_FILENAME | FilesystemIterator::CURRENT_AS_PATHNAME);
/// foreach ($it as $k => $v) var_dump($k, str_replace($d, '', $v));
/// var_dump($it->getFlags());
/// ```
/// ```expect
/// string(6) "/f.txt"
/// string(11) "SplFileInfo"
/// string(5) "f.txt"
/// string(1) "."
/// string(2) "/."
/// string(2) ".."
/// string(3) "/.."
/// string(5) "f.txt"
/// string(6) "/f.txt"
/// int(288)
/// ```
fn fs_construct(req: &mut Request) -> Result<Value, Thrown> {
    dir_construct_with(req, true, SKIP_DOTS)?;
    Ok(Value::null())
}

/// `FilesystemIterator::key(): string`, by the key flags.
fn fs_key(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let (flags, entry) = fs(req, this, |it| (it.flags, it.entry.clone()))?;
    if flags & KEY_AS_FILENAME != 0 {
        return crate::string_value(req, &entry);
    }
    let name = file_name(req, this)?;
    crate::string_value(req, &name)
}

/// `FilesystemIterator::current()`, by the current flags: the path
/// name, an `SplFileInfo`, or the iterator itself.
fn fs_current(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let flags = fs(req, this, |it| it.flags)?;
    if flags & CURRENT_AS_PATHNAME != 0 {
        let name = file_name(req, this)?;
        return crate::string_value(req, &name);
    }
    if flags & CURRENT_AS_SELF != 0 {
        return Ok(crate::copied(req, this.as_value()));
    }
    let name = file_name(req, this)?;
    let chosen = fs(req, this, |it| it.info_class)?;
    let Some(class) = class_or(req, b"SplFileInfo", chosen)? else {
        return Ok(Value::null());
    };
    create_info(req, &name, class)
}

/// `getFlags(): int`, the flags under `SPL_FILE_DIR_KEY_MODE_MASK`,
/// `SPL_FILE_DIR_CURRENT_MODE_MASK` and `SPL_FILE_DIR_OTHERS_MASK`.
///
/// ```php
/// $it = new FilesystemIterator(__DIR__, -1);
/// printf("%08X %d %d\n", $it->getFlags(), FilesystemIterator::FOLLOW_SYMLINKS, FilesystemIterator::OTHER_MODE_MASK);
/// ```
/// ```expect
/// 00007FF0 16384 28672
/// ```
fn fs_get_flags(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let mask = KEY_MODE_MASK | CURRENT_MODE_MASK | OTHER_MODE_MASK;
    Ok(Value::int(fs(req, this, |it| it.flags & mask)?))
}

/// `setFlags(int $flags): void`.
fn fs_set_flags(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let flags = crate::long(req, 1)?;
    let mask = KEY_MODE_MASK | CURRENT_MODE_MASK | OTHER_MODE_MASK;
    fs(req, this, |it| {
        it.flags = (it.flags & !mask) | (flags & mask);
    })?;
    Ok(Value::null())
}

/// `RecursiveDirectoryIterator::__construct(string $directory, int
/// $flags = KEY_AS_PATHNAME | CURRENT_AS_FILEINFO)`.
///
/// ```php
/// $d = __DIR__ . '/spl-rec'; @mkdir("$d/sub", 0777, true); touch("$d/sub/leaf.txt");
/// $it = new RecursiveIteratorIterator(new RecursiveDirectoryIterator($d, FilesystemIterator::SKIP_DOTS));
/// foreach ($it as $path => $info) var_dump(str_replace($d, '', $path), $it->getInnerIterator()->getSubPath(), $it->getInnerIterator()->getSubPathname());
/// ```
/// ```expect
/// string(13) "/sub/leaf.txt"
/// string(3) "sub"
/// string(12) "sub/leaf.txt"
/// ```
fn recursive_construct(req: &mut Request) -> Result<Value, Thrown> {
    dir_construct_with(req, true, 0)?;
    Ok(Value::null())
}

/// `hasChildren(bool $allowLinks = false): bool`: a directory that is no
/// dot, a link only where allowed or `FOLLOW_SYMLINKS` is set.
fn recursive_has_children(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    let this = crate::this(req)?;
    let links = crate::bool_or(req, 1, false)?;
    let (entry, flags) = fs(req, this, |it| (it.entry.clone(), it.flags))?;
    if entry.is_empty() || is_dot(&entry) {
        return Ok(Value::bool(false));
    }
    let name = file_name(req, this)?;
    let Some(lstat) = req.url_stat(&name, true, true)? else {
        return Ok(Value::bool(false));
    };
    if lstat[2] & 0o170_000 != 0o120_000 {
        return Ok(Value::bool(lstat[2] & 0o170_000 == 0o040_000));
    }
    if !links && flags & FOLLOW_SYMLINKS == 0 {
        return Ok(Value::bool(false));
    }
    let stat = req.url_stat(&name, false, true)?;
    Ok(Value::bool(
        stat.is_some_and(|s| s[2] & 0o170_000 == 0o040_000),
    ))
}

/// `getChildren(): RecursiveDirectoryIterator`: a new iterator of this
/// class over the entry, its sub path one level on.
fn recursive_get_children(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let name = file_name(req, this)?;
    let (flags, entry, sub, info, file) = fs(req, this, |it| {
        (
            it.flags,
            it.entry.clone(),
            it.sub_path.clone(),
            it.info_class,
            it.file_class,
        )
    })?;
    let arg = crate::string_value(req, &name)?;
    let made = instantiate(req, this.class(), &[arg, Value::int(flags)]);
    req.release(arg);
    let child = made?;
    let sub_path = match sub {
        Some(sub) if !sub.is_empty() => [sub.as_slice(), b"/", entry.as_slice()].concat(),
        _ => entry,
    };
    fs(req, child, |it| {
        it.sub_path = Some(sub_path);
        it.info_class = info;
        it.file_class = file;
    })?;
    Ok(child.as_value())
}

/// `getSubPath(): string`.
fn recursive_get_sub_path(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let sub = fs(req, this, |it| it.sub_path.clone())?.unwrap_or_default();
    crate::string_value(req, &sub)
}

/// `getSubPathname(): string`.
fn recursive_get_sub_pathname(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let (sub, entry) = fs(req, this, |it| (it.sub_path.clone(), it.entry.clone()))?;
    let out = match sub {
        Some(sub) => [sub.as_slice(), b"/", entry.as_slice()].concat(),
        None => entry,
    };
    crate::string_value(req, &out)
}

/// `GlobIterator::__construct(string $pattern, int $flags =
/// KEY_AS_PATHNAME | CURRENT_AS_FILEINFO)`: the pattern expanded through
/// `ext/standard`'s `glob`, as the `glob://` wrapper expands it.
fn glob_construct(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 2)?;
    let this = crate::this(req)?;
    let pattern = path_argument(req, 1)?;
    let flags = crate::long_or(req, 2, KEY_AS_PATHNAME | CURRENT_AS_FILEINFO)?;
    if pattern.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    let bare = pattern
        .strip_prefix(b"glob://")
        .unwrap_or(&pattern)
        .to_vec();
    let arg = crate::string_value(req, &bare)?;
    let found = crate::standard(req, b"glob", &[arg]);
    req.release(arg);
    let found = found?;
    let entries: Vec<Vec<u8>> = found
        .as_array()
        .map(|a| {
            a.snapshot()
                .into_iter()
                .filter_map(|(_, v)| v.as_str_bytes().map(<[u8]>::to_vec))
                .collect()
        })
        .unwrap_or_default();
    req.release(found);
    fs(req, this, |it| {
        it.kind = Type::Dir;
        it.flags = flags;
        it.path = Some(dirname(&bare));
        it.glob = Some((bare, entries));
        it.index = 0;
    })?;
    dir_read(req, this)?;
    Ok(Value::null())
}

/// `GlobIterator::count(): int`.
fn glob_count(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    match fs(req, this, |it| it.glob.as_ref().map(|(_, e)| e.len()))? {
        Some(n) => Ok(Value::int(i64::try_from(n).unwrap_or(i64::MAX))),
        None => Err(throw(req, "Error", "GlobIterator is not initialized")),
    }
}

// ---- the file objects

/// `spl_filesystem_file_open`: a directory refused, the stream opened
/// with warnings turned into php's `RuntimeException`, and the CSV
/// controls at their defaults.
fn file_open(
    req: &mut Request,
    this: PhpObject,
    name: &[u8],
    mode: &[u8],
    include: bool,
) -> Result<(), Thrown> {
    fs(req, this, |it| it.kind = Type::File)?;
    if req
        .url_stat(name, false, true)?
        .is_some_and(|s| s[2] & 0o170_000 == 0o040_000)
    {
        return Err(throw(
            req,
            "LogicException",
            "Cannot use SplFileObject with directories",
        ));
    }
    let opened = php_ext::raising::throwing_warnings(req, "RuntimeException", |req| {
        req.open_stream(name, mode, include)
    })?;
    let Some(stream) = opened.filter(|_| !name.is_empty()) else {
        let shown = String::from_utf8_lossy(name).into_owned();
        return Err(throw(
            req,
            "RuntimeException",
            &format!("Cannot open file '{shown}'"),
        ));
    };
    let trimmed = if name.len() > 1 && slash(name[name.len() - 1]) {
        &name[..name.len() - 1]
    } else {
        name
    };
    fs(req, this, |it| {
        it.stream = Some(stream);
        it.file_name = Some(trimmed.to_vec());
        it.orig_path = Some(name.to_vec());
        it.open_mode = mode.to_vec();
        it.delimiter = b',';
        it.enclosure = b'"';
        it.escape = Some(b'\\');
        it.escape_set = false;
    })
}

/// `SplFileObject::__construct()`: the file name, then the `fopen()`
/// letters, `"r"` by default, whether to search the include path,
/// `false` by default, and the stream's options, `null` by default.
///
/// ```php
/// $f = __DIR__ . '/spl-file.txt'; file_put_contents($f, "a\n\nb");
/// $o = new SplFileObject($f);
/// $o->setFlags(SplFileObject::DROP_NEW_LINE | SplFileObject::SKIP_EMPTY | SplFileObject::READ_AHEAD);
/// foreach ($o as $n => $line) echo "$n=$line;";
/// echo "\n";
/// try { new SplFileObject(__DIR__ . '/nope.txt'); } catch (RuntimeException $e) { echo str_replace(__DIR__, '', $e->getMessage()), "\n"; }
/// var_dump($o->getFilename(), str_replace(__DIR__, '', $o->getPath()), $o->getFlags());
/// try { clone $o; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 0=a;1=b;
/// SplFileObject::__construct(/nope.txt): Failed to open stream: No such file or directory
/// string(12) "spl-file.txt"
/// string(0) ""
/// int(7)
/// Trying to clone an uncloneable object of class SplFileObject
/// ```
fn file_construct(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 4)?;
    let this = crate::this(req)?;
    let name = path_argument(req, 1)?;
    let mode = match crate::given(req, 2) {
        Some(_) => crate::string(req, 2)?.as_bytes().to_vec(),
        None => b"r".to_vec(),
    };
    let include = crate::bool_or(req, 3, false)?;
    if fs(req, this, |it| it.stream.is_some())? {
        return Err(throw(req, "Error", "Cannot call constructor twice"));
    }
    file_open(req, this, &name, &mode, include)?;
    let mut len = name.len();
    if len > 1 && slash(name[len - 1]) {
        len -= 1;
    }
    while len > 1 && !slash(name[len - 1]) {
        len -= 1;
    }
    len = len.saturating_sub(1);
    let path = name[..len].to_vec();
    fs(req, this, |it| it.path = Some(path))?;
    Ok(Value::null())
}

/// `SplTempFileObject::__construct(int $maxMemory = 2 * 1024 * 1024)`:
/// `php://temp`, `php://temp/maxmemory:N` or `php://memory`.
///
/// ```php
/// $t = new SplTempFileObject();
/// $t->fwrite("x\ny\n"); $t->rewind();
/// foreach ($t as $n => $line) echo $n, json_encode($line), ";";
/// echo "\n";
/// var_dump($t->getPathname(), $t->getFilename(), (new SplTempFileObject(-1))->getPathname(), (new SplTempFileObject(10))->getPathname());
/// ```
/// ```expect
/// 0"x\n";1"y\n";
/// string(10) "php://temp"
/// string(10) "php://temp"
/// string(12) "php://memory"
/// string(23) "php://temp/maxmemory:10"
/// ```
fn temp_construct(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    let this = crate::this(req)?;
    let max = crate::long_or(req, 1, 2 * 1024 * 1024)?;
    if fs(req, this, |it| it.stream.is_some())? {
        return Err(throw(req, "Error", "Cannot call constructor twice"));
    }
    let name: Vec<u8> = if max < 0 {
        b"php://memory".to_vec()
    } else if req.argument_count() > 0 {
        format!("php://temp/maxmemory:{max}").into_bytes()
    } else {
        b"php://temp".to_vec()
    };
    file_open(req, this, &name, b"wb", false)?;
    fs(req, this, |it| it.path = Some(Vec::new()))?;
    Ok(Value::null())
}

/// `CHECK_SPL_FILE_OBJECT_IS_INITIALIZED`: the receiver and its stream.
fn this_file(req: &mut Request) -> Result<(PhpObject, Stream), Thrown> {
    let this = crate::this(req)?;
    match fs(req, this, |it| it.stream)? {
        Some(stream) => Ok((this, stream)),
        None => Err(uninitialized(req)),
    }
}

/// `spl_filesystem_file_free_line`.
fn free_line(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    if let Some(csv) = fs(req, this, |it| {
        it.line = None;
        it.csv.take()
    })? {
        req.release(csv);
    }
    Ok(())
}

fn cannot_read(req: &mut Request, this: PhpObject) -> Result<Thrown, Thrown> {
    let name =
        String::from_utf8_lossy(&fs(req, this, |it| it.file_name.clone())?.unwrap_or_default())
            .into_owned();
    Ok(throw(
        req,
        "RuntimeException",
        &format!("Cannot read from file {name}"),
    ))
}

/// `spl_filesystem_file_read_ex`: one line, its newline dropped under
/// `DROP_NEW_LINE`, `false` at the end.
fn read_line_raw(
    req: &mut Request,
    this: PhpObject,
    silent: bool,
    line_add: i64,
    csv: bool,
) -> Result<bool, Thrown> {
    free_line(req, this)?;
    let (stream, max, flags) = fs(req, this, |it| (it.stream, it.max_line_len, it.flags))?;
    let Some(stream) = stream else {
        return Err(uninitialized(req));
    };
    if stream.eof(req) {
        if !silent {
            return Err(cannot_read(req, this)?);
        }
        return Ok(false);
    }
    let max = usize::try_from(max).ok().filter(|m| *m > 0).map(|m| m + 1);
    let line = stream.get_line(req, max)?;
    let mut line = line.map(|l| l.as_bytes().to_vec()).unwrap_or_default();
    if !csv && flags & DROP_NEW_LINE != 0 && line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    fs(req, this, |it| {
        it.line = Some(line);
        it.line_num += line_add;
    })?;
    Ok(true)
}

fn read_line_counted(
    req: &mut Request,
    this: PhpObject,
    silent: bool,
    csv: bool,
) -> Result<bool, Thrown> {
    let add = i64::from(fs(req, this, |it| it.line.is_some())?);
    read_line_raw(req, this, silent, add, csv)
}

fn line_empty(req: &mut Request, this: PhpObject) -> Result<bool, Thrown> {
    fs(req, this, |it| {
        let line = it.line.as_deref().unwrap_or_default();
        line.is_empty()
            || (it.flags & READ_CSV != 0
                && it.flags & DROP_NEW_LINE != 0
                && (line == b"\n" || line == b"\r\n"))
    })
}

/// `spl_filesystem_file_read_csv`: a line read and handed to
/// `ext/standard`'s `str_getcsv`, php's `php_fgetcsv` over the line.
fn read_csv(
    req: &mut Request,
    this: PhpObject,
    silent: bool,
    control: (u8, u8, Option<u8>),
) -> Result<bool, Thrown> {
    loop {
        if !read_line_counted(req, this, silent, true)? {
            return Ok(false);
        }
        let skip = fs(req, this, |it| it.flags & SKIP_EMPTY != 0)?;
        if !(skip && line_empty(req, this)?) {
            break;
        }
    }
    let mut line = fs(req, this, |it| it.line.clone())?.unwrap_or_default();
    // `php_fgetcsv` reads on from the stream while a field's enclosure
    // is open at the end of the line, so a quoted field spans lines.
    while open_enclosure(&line, control) {
        let Some(stream) = fs(req, this, |it| it.stream)? else {
            break;
        };
        if stream.eof(req) {
            break;
        }
        match stream.get_line(req, None)? {
            Some(more) if !more.as_bytes().is_empty() => line.extend_from_slice(more.as_bytes()),
            _ => break,
        }
    }
    fs(req, this, |it| it.line = Some(line.clone()))?;
    let line = if line.ends_with(b"\n") {
        let mut l = line;
        l.pop();
        if l.ends_with(b"\r") {
            l.pop();
        }
        l
    } else {
        line
    };
    let args = [
        crate::string_value(req, &line)?,
        crate::string_value(req, &[control.0])?,
        crate::string_value(req, &[control.1])?,
        crate::string_value(req, control.2.as_slice())?,
    ];
    let parsed = crate::standard(req, b"str_getcsv", &args);
    for arg in args {
        req.release(arg);
    }
    let parsed = parsed?;
    if let Some(old) = fs(req, this, |it| it.csv.replace(parsed))? {
        req.release(old);
    }
    Ok(true)
}

/// Whether `line` ends inside an enclosed field, as `php_fgetcsv`'s
/// state stands at the end of its buffer: an enclosure that opens a
/// field opens it, a doubled one inside stands for itself, the escape takes
/// the byte after it, and a lone one closes it.
fn open_enclosure(line: &[u8], (delimiter, enclosure, escape): (u8, u8, Option<u8>)) -> bool {
    let mut at = 0;
    let mut inside = false;
    let mut field_start = true;
    while let Some(&c) = line.get(at) {
        if inside {
            if Some(c) == escape && escape != Some(enclosure) {
                at += 2;
                continue;
            }
            if c == enclosure {
                if line.get(at + 1) == Some(&enclosure) {
                    at += 2;
                    continue;
                }
                inside = false;
            }
        } else if field_start && c == enclosure {
            inside = true;
            field_start = false;
        } else if c == delimiter {
            field_start = true;
        } else if c != b' ' && c != b'\t' {
            field_start = false;
        }
        at += 1;
    }
    inside
}

/// `spl_filesystem_file_read_line_ex`: CSV, a user `getCurrentLine()`,
/// or a line read directly.
fn read_line_ex(req: &mut Request, this: PhpObject, silent: bool) -> Result<bool, Thrown> {
    let (flags, control) = fs(req, this, |it| {
        (it.flags, (it.delimiter, it.enclosure, it.escape))
    })?;
    if flags & READ_CSV != 0 {
        return read_csv(req, this, silent, control);
    }
    if crate::overridden(req, this, b"getcurrentline") {
        free_line(req, this)?;
        let Some(stream) = fs(req, this, |it| it.stream)? else {
            return Err(uninitialized(req));
        };
        if stream.eof(req) {
            if !silent {
                return Err(cannot_read(req, this)?);
            }
            return Ok(false);
        }
        let answered = call_method(req, &this, b"getCurrentLine", &[])?;
        let Some(line) = answered.as_str_bytes().map(<[u8]>::to_vec) else {
            let class = String::from_utf8_lossy(&crate::class_name(req, this.class())).into_owned();
            let name = php_ext::coerce::value_name(req, &answered);
            req.release(answered);
            return Err(throw(
                req,
                "TypeError",
                &format!(
                    "{class}::getCurrentLine(): Return value must be of type string, {name} returned"
                ),
            ));
        };
        req.release(answered);
        fs(req, this, |it| {
            if it.line.is_some() || it.csv.is_some() {
                it.line_num += 1;
            }
            it.line = Some(line);
        })?;
        return Ok(true);
    }
    read_line_counted(req, this, silent, false)
}

/// `spl_filesystem_file_read_line`: empty lines stepped over under
/// `SKIP_EMPTY`.
fn read_line(req: &mut Request, this: PhpObject, silent: bool) -> Result<bool, Thrown> {
    let mut read = read_line_ex(req, this, silent)?;
    while read && fs(req, this, |it| it.flags & SKIP_EMPTY != 0)? && line_empty(req, this)? {
        free_line(req, this)?;
        read = read_line_ex(req, this, silent)?;
    }
    Ok(read)
}

/// `spl_filesystem_file_rewind`.
fn file_rewind_to_start(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    let Some(stream) = fs(req, this, |it| it.stream)? else {
        return Err(uninitialized(req));
    };
    if !stream.seek(req, 0, Whence::Set) {
        let name =
            String::from_utf8_lossy(&fs(req, this, |it| it.file_name.clone())?.unwrap_or_default())
                .into_owned();
        return Err(throw(
            req,
            "RuntimeException",
            &format!("Cannot rewind file {name}"),
        ));
    }
    free_line(req, this)?;
    fs(req, this, |it| it.line_num = 0)?;
    if fs(req, this, |it| it.flags & READ_AHEAD != 0)? {
        read_line(req, this, true)?;
    }
    Ok(())
}

/// `rewind(): void`.
fn file_rewind(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    file_rewind_to_start(req, this)?;
    Ok(Value::null())
}

/// `eof(): bool`.
fn file_eof(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let (_, stream) = this_file(req)?;
    Ok(Value::bool(stream.eof(req)))
}

/// `valid(): bool`: a line read ahead under `READ_AHEAD`, and not at
/// the end otherwise.
fn file_valid(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let (flags, has, stream) = fs(req, this, |it| {
        (it.flags, it.line.is_some() || it.csv.is_some(), it.stream)
    })?;
    if flags & READ_AHEAD != 0 {
        return Ok(Value::bool(has));
    }
    Ok(Value::bool(stream.is_some_and(|s| !s.eof(req))))
}

/// `fgets(): string`, and `getCurrentLine()`.
///
/// ```php
/// $t = new SplTempFileObject(); $t->fwrite("1\n2"); $t->rewind();
/// var_dump($t->fgets(), $t->key(), $t->fgets(), $t->eof());
/// try { $t->fgets(); } catch (RuntimeException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(2) "1
/// "
/// int(1)
/// string(1) "2"
/// bool(true)
/// Cannot read from file php://temp
/// ```
fn file_fgets(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let (this, _) = this_file(req)?;
    read_line_raw(req, this, false, 1, false)?;
    let line = fs(req, this, |it| it.line.clone())?.unwrap_or_default();
    crate::string_value(req, &line)
}

/// `fread(int $length): string|false`.
///
/// ```php
/// $t = new SplTempFileObject(); var_dump($t->fwrite("abcdef", 4), $t->ftell(), $t->fseek(1), $t->fread(2), $t->fgetc(), $t->fgetc());
/// try { $t->fread(0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// $t->rewind(); var_dump($t->fpassthru());
/// ```
/// ```expect
/// int(4)
/// int(4)
/// int(0)
/// string(2) "bc"
/// string(1) "d"
/// bool(false)
/// SplFileObject::fread(): Argument #1 ($length) must be greater than 0
/// abcdint(4)
/// ```
fn file_fread(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let (_, stream) = this_file(req)?;
    let len = crate::long(req, 1)?;
    if len <= 0 {
        return Err(arg_value_error(req, 1, "be greater than 0"));
    }
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    Ok(match stream.read(req, len)? {
        Some(s) => Value::string(s),
        None => Value::bool(false),
    })
}

/// The one-byte CSV control at `position`, php's `ValueError` for one of
/// another length.
fn control_char(req: &mut Request, position: u32, default: u8) -> Result<u8, Thrown> {
    match crate::given(req, position) {
        Some(_) => {
            let given = crate::string(req, position)?.as_bytes().to_vec();
            if given.len() != 1 {
                return Err(arg_value_error(req, position, "be a single character"));
            }
            Ok(given[0])
        }
        None => Ok(default),
    }
}

/// The escape argument at `position`: empty for none, one byte, and
/// php's deprecation where the call leaves it to its default.
fn escape_arg(req: &mut Request, position: u32, this: PhpObject) -> Result<Option<u8>, Thrown> {
    if crate::given(req, position).is_some() {
        let given = crate::string(req, position)?.as_bytes().to_vec();
        return match given.len() {
            0 => Ok(None),
            1 => Ok(Some(given[0])),
            _ => Err(arg_value_error(
                req,
                position,
                "be empty or a single character",
            )),
        };
    }
    let (set, escape) = fs(req, this, |it| (it.escape_set, it.escape))?;
    if !set {
        php_ext::raising::docref(
            req,
            php_ext::raising::Level::Deprecated,
            "the $escape parameter must be provided, as its default value will change, either explicitly or via SplFileObject::setCsvControl()",
        );
    }
    Ok(escape)
}

/// `fgetcsv(string $separator = ",", string $enclosure = "\"", string
/// $escape = "\\"): array|false`, through `ext/standard`'s `str_getcsv`,
/// a field whose enclosure is open at the end of the line read on
/// into the next.
///
/// ```php
/// $t = new SplTempFileObject();
/// $t->fwrite("a,\"b\nc\",d\ne,f\n");
/// $t->rewind();
/// while ($row = $t->fgetcsv(',', '"', '')) echo json_encode($row), ' ', $t->key(), "\n";
/// ```
/// ```expect
/// ["a","b\nc","d"] 0
/// ["e","f"] 1
/// ```
fn file_fgetcsv(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 3)?;
    let (this, _) = this_file(req)?;
    let (d, e) = fs(req, this, |it| (it.delimiter, it.enclosure))?;
    let delimiter = control_char(req, 1, d)?;
    let enclosure = control_char(req, 2, e)?;
    let escape = escape_arg(req, 3, this)?;
    if !read_csv(req, this, true, (delimiter, enclosure, escape))? {
        return Ok(Value::bool(false));
    }
    let csv = fs(req, this, |it| it.csv)?;
    Ok(csv.map_or_else(|| Value::bool(false), |c| crate::copied(req, c)))
}

/// `fputcsv(array $fields, ...): int|false`, through `ext/standard`'s
/// `fputcsv` on the stream.
fn file_fputcsv(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 5)?;
    let (this, stream) = this_file(req)?;
    let fields = crate::array(req, 1)?;
    let (d, e) = fs(req, this, |it| (it.delimiter, it.enclosure))?;
    let delimiter = control_char(req, 2, d)?;
    let enclosure = control_char(req, 3, e)?;
    let escape = escape_arg(req, 4, this)?;
    let eol = match crate::given(req, 5) {
        Some(_) => crate::string(req, 5)?.as_bytes().to_vec(),
        None => b"\n".to_vec(),
    };
    let args = [
        stream.as_value(),
        Value::array(fields),
        crate::string_value(req, &[delimiter])?,
        crate::string_value(req, &[enclosure])?,
        crate::string_value(req, escape.as_slice())?,
        crate::string_value(req, &eol)?,
    ];
    let answered = crate::standard(req, b"fputcsv", &args);
    for arg in &args[2..] {
        req.release(*arg);
    }
    answered
}

/// `setCsvControl(string $separator = ",", string $enclosure = "\"",
/// string $escape = "\\"): void`.
///
/// ```php
/// $t = new SplTempFileObject();
/// $t->setCsvControl(';', "'", '');
/// var_dump($t->getCsvControl());
/// try { $t->setCsvControl('ab'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   string(1) ";"
///   [1]=>
///   string(1) "'"
///   [2]=>
///   string(0) ""
/// }
/// SplFileObject::setCsvControl(): Argument #1 ($separator) must be a single character
/// ```
fn file_set_csv_control(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 3)?;
    let this = crate::this(req)?;
    let delimiter = control_char(req, 1, b',')?;
    let enclosure = control_char(req, 2, b'"')?;
    let (escape, set) = match crate::given(req, 3) {
        Some(_) => {
            let given = crate::string(req, 3)?.as_bytes().to_vec();
            match given.len() {
                0 => (None, true),
                1 => (Some(given[0]), true),
                _ => return Err(arg_value_error(req, 3, "be empty or a single character")),
            }
        }
        None => (Some(b'\\'), false),
    };
    fs(req, this, |it| {
        it.delimiter = delimiter;
        it.enclosure = enclosure;
        it.escape = escape;
        if set {
            it.escape_set = true;
        }
    })?;
    Ok(Value::null())
}

/// `getCsvControl(): array`.
fn file_get_csv_control(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let (d, e, x) = fs(req, this, |it| (it.delimiter, it.enclosure, it.escape))?;
    let mut out = PhpArray::new(req)?;
    for bytes in [vec![d], vec![e], x.map(|x| vec![x]).unwrap_or_default()] {
        let value = crate::string_value(req, &bytes)?;
        out.append(req, value)?;
    }
    Ok(Value::array(out))
}

/// `flock(int $operation, &$wouldBlock = null): bool`.
fn file_flock(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 2)?;
    let (_, stream) = this_file(req)?;
    let operation = crate::long(req, 1)?;
    if !(1..=3).contains(&(operation & 3)) {
        return Err(arg_value_error(
            req,
            1,
            "be one of LOCK_SH, LOCK_EX, or LOCK_UN",
        ));
    }
    let native = [0, 1, 2, 8][usize::try_from(operation & 3).unwrap_or(0)] | (operation & 4);
    Ok(Value::bool(stream.flock(req, native)))
}

/// `fflush(): bool`.
fn file_fflush(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let (_, stream) = this_file(req)?;
    Ok(Value::bool(stream.flush(req)))
}

/// `ftell(): int|false`.
fn file_ftell(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let (_, stream) = this_file(req)?;
    let at = stream.tell(req);
    Ok(if at == -1 {
        Value::bool(false)
    } else {
        Value::int(at)
    })
}

/// `fseek(int $offset, int $whence = SEEK_SET): int`, `0` or `-1`.
fn file_fseek(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 2)?;
    let (this, stream) = this_file(req)?;
    let offset = crate::long(req, 1)?;
    let whence = crate::long_or(req, 2, 0)?;
    free_line(req, this)?;
    Ok(Value::int(
        if stream.seek(req, offset, Whence::from_int(whence)) {
            0
        } else {
            -1
        },
    ))
}

/// `fgetc(): string|false`: one byte, the line count moved on a `\n`.
fn file_fgetc(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let (this, stream) = this_file(req)?;
    free_line(req, this)?;
    match stream.read(req, 1)? {
        Some(s) if !s.is_empty() => {
            if s.as_bytes() == b"\n" {
                fs(req, this, |it| it.line_num += 1)?;
            }
            Ok(Value::string(s))
        }
        Some(s) => {
            req.release(Value::string(s));
            Ok(Value::bool(false))
        }
        None => Ok(Value::bool(false)),
    }
}

/// `fpassthru(): int`, `php_stream_passthru`: the rest written out.
fn file_fpassthru(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let (_, stream) = this_file(req)?;
    let rest = stream.copy_to_mem(req, None)?;
    let n = rest.len();
    req.write(rest.as_bytes());
    req.release(Value::string(rest));
    Ok(Value::int(i64::try_from(n).unwrap_or(i64::MAX)))
}

/// `fscanf(string $format, mixed &...$vars): array|int|null`: the next
/// line through `ext/standard`'s `sscanf`.
///
/// ```php
/// $t = new SplTempFileObject(); $t->fwrite("7 apples\n"); $t->rewind();
/// var_dump($t->fscanf('%d %s'));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(7)
///   [1]=>
///   string(6) "apples"
/// }
/// ```
fn file_fscanf(req: &mut Request) -> Result<Value, Thrown> {
    let (this, _) = this_file(req)?;
    if req.argument_count() < 1 {
        crate::arity(req, 1, usize::MAX)?;
    }
    let format = crate::string(req, 1)?;
    read_line_counted(req, this, false, false)?;
    let line = fs(req, this, |it| it.line.clone())?.unwrap_or_default();
    let line = crate::string_value(req, &line)?;
    let mut args = vec![line, Value::string(format)];
    for at in 2..=u32::try_from(req.argument_count()).unwrap_or(1) {
        args.push(req.argument(at).unwrap_or_default());
    }
    let answered = crate::standard(req, b"sscanf", &args);
    req.release(line);
    answered
}

/// `fwrite(string $data, ?int $length = null): int|false`.
fn file_fwrite(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 2)?;
    let (_, stream) = this_file(req)?;
    let data = crate::string(req, 1)?.as_bytes().to_vec();
    let len = match crate::given(req, 2).map(|v| v.deref()) {
        Some(v) if !matches!(v.as_ref(), ValueRef::Null) => {
            let n = crate::long(req, 2)?;
            usize::try_from(n).unwrap_or(0).min(data.len())
        }
        _ => data.len(),
    };
    if len == 0 {
        return Ok(Value::int(0));
    }
    Ok(match stream.write(req, &data[..len]) {
        Some(n) => Value::int(i64::try_from(n).unwrap_or(i64::MAX)),
        None => Value::bool(false),
    })
}

/// `fstat(): array`, through `ext/standard`'s `fstat` on the stream.
fn file_fstat(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let (_, stream) = this_file(req)?;
    crate::standard(req, b"fstat", &[stream.as_value()])
}

/// `ftruncate(int $size): bool`.
///
/// ```php
/// $t = new SplTempFileObject(); $t->fwrite("abcdef");
/// var_dump($t->ftruncate(2), $t->fstat()['size']);
/// try { $t->ftruncate(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(2)
/// SplFileObject::ftruncate(): Argument #1 ($size) must be greater than or equal to 0
/// ```
fn file_ftruncate(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let (this, stream) = this_file(req)?;
    let size = crate::long(req, 1)?;
    if size < 0 {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    }
    if let Some(done) = stream.truncate(req, usize::try_from(size).unwrap_or(0)) {
        return Ok(Value::bool(done));
    }
    let name =
        String::from_utf8_lossy(&fs(req, this, |it| it.file_name.clone())?.unwrap_or_default())
            .into_owned();
    Err(throw(
        req,
        "LogicException",
        &format!("Can't truncate file {name}"),
    ))
}

/// `current(): string|array|false`: the line, read where none is, or
/// its fields under `READ_CSV`.
fn file_current(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let (this, _) = this_file(req)?;
    if fs(req, this, |it| it.line.is_none() && it.csv.is_none())? {
        read_line(req, this, true)?;
    }
    let (line, csv, flags) = fs(req, this, |it| (it.line.clone(), it.csv, it.flags))?;
    if let Some(line) = line
        && (flags & READ_CSV == 0 || csv.is_none())
    {
        return crate::string_value(req, &line);
    }
    Ok(csv.map_or_else(|| Value::bool(false), |c| crate::copied(req, c)))
}

/// `key(): int`, the line number.
fn file_key(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    Ok(Value::int(fs(req, this, |it| it.line_num)?))
}

/// `next(): void`.
fn file_next(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let (this, _) = this_file(req)?;
    free_line(req, this)?;
    if fs(req, this, |it| it.flags & READ_AHEAD != 0)? {
        read_line(req, this, true)?;
    }
    fs(req, this, |it| it.line_num += 1)?;
    Ok(Value::null())
}

/// `setFlags(int $flags): void`.
fn file_set_flags(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let flags = crate::long(req, 1)?;
    fs(req, this, |it| it.flags = flags)?;
    Ok(Value::null())
}

/// `getFlags(): int`.
fn file_get_flags(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    Ok(Value::int(fs(req, this, |it| it.flags & 0xF)?))
}

/// `setMaxLineLen(int $maxLength): void`.
///
/// ```php
/// $t = new SplTempFileObject(); $t->fwrite("abcdef\n"); $t->rewind();
/// $t->setMaxLineLen(3); var_dump($t->getMaxLineLen(), $t->current());
/// try { $t->setMaxLineLen(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($t->hasChildren(), $t->getChildren());
/// ```
/// ```expect
/// int(3)
/// string(3) "abc"
/// SplFileObject::setMaxLineLen(): Argument #1 ($maxLength) must be greater than or equal to 0
/// bool(false)
/// NULL
/// ```
fn file_set_max_line_len(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let max = crate::long(req, 1)?;
    if max < 0 {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    }
    fs(req, this, |it| it.max_line_len = max)?;
    Ok(Value::null())
}

/// `getMaxLineLen(): int`.
fn file_get_max_line_len(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    Ok(Value::int(fs(req, this, |it| it.max_line_len)?))
}

/// `hasChildren(): false`.
fn file_has_children(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    Ok(Value::bool(false))
}

/// `getChildren(): null`.
fn file_get_children(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    Ok(Value::null())
}

/// `seek(int $line): void`: rewound and read `line` lines on.
///
/// ```php
/// $t = new SplTempFileObject(); $t->fwrite("a\nb\nc\n");
/// $t->seek(1); var_dump($t->key(), $t->current());
/// $t->seek(10); var_dump($t->key(), $t->current());
/// ```
/// ```expect
/// int(1)
/// string(2) "b
/// "
/// int(2)
/// bool(false)
/// ```
fn file_seek(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let (this, _) = this_file(req)?;
    let line = crate::long(req, 1)?;
    if line < 0 {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    }
    file_rewind_to_start(req, this)?;
    for _ in 0..line {
        if !read_line(req, this, true)? {
            return Ok(Value::null());
        }
    }
    if line > 0 && fs(req, this, |it| it.flags & READ_AHEAD == 0)? {
        fs(req, this, |it| it.line_num += 1)?;
        free_line(req, this)?;
    }
    Ok(Value::null())
}

/// `__toString(): string`, the current line.
fn file_to_string(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let (this, _) = this_file(req)?;
    if fs(req, this, |it| it.line.is_none())? {
        read_line(req, this, false)?;
    }
    let line = fs(req, this, |it| it.line.clone())?.unwrap_or_default();
    crate::string_value(req, &line)
}
