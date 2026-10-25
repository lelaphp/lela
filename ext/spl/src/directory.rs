//! `spl_directory.c`: `SplFileInfo`, `DirectoryIterator`,
//! `FilesystemIterator`, `RecursiveDirectoryIterator`, `GlobIterator`,
//! `SplFileObject` and `SplTempFileObject` over one payload, php's
//! `spl_filesystem_object` ([`Fs`]), and the engine's streams layer: a
//! directory is a stream `opendir` opens and `readdir` reads, a file a
//! stream `fopen` opens, and a stat the wrapper's `url_stat`. The CSV
//! methods, `fscanf()`, `fstat()` and the access tests call
//! `ext/standard`'s `php_fgetcsv`, `php_fputcsv`, `php_sscanf_internal`,
//! `php_fstat` and `php_stat` directly, as `spl_directory.c` does, never a
//! native through the function table. A `GlobIterator`, and a
//! `DirectoryIterator` over `glob://`, walk the glob stream `opendir`
//! opens, whose path and count they read off it.

use php_ext::abi::{
    CloneObj, DtorObj, ExtensionBuilder, False, Instance, Null, ObjectHandlers, OrFalse, Passed,
    Path as PathArg, Raw, Ref, Refusals, Std, This, Union, Untyped, Variadic,
};
use php_ext::attr::php_class;
use php_ext::call::{Function, autoload, call_method, instantiate, lookup_class, new_object};
use php_ext::coerce::coerce_param;
use php_ext::decl::Type as DeclType;
use php_ext::raising::{
    Thrown, arg_type_error, arg_value_error, throw, throw_error, throw_exception_ex, type_error,
};
use php_ext::request::{OwnedStream, Request, Stream, Streams as _, Whence};
use php_ext::values::{ClassId, Owner, PhpArray, PhpObject, PhpString, Resource, Value, ValueRef};

use crate::{Countable, RecursiveIterator, SeekableIterator, Stringable};

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
    /// `oth` with its `oth_handler`: what a class extending these lays on
    /// the object ([`Other`]).
    oth: Option<Box<dyn Other>>,
    kind: Type,
    path: Option<Vec<u8>>,
    file_name: Option<Vec<u8>>,
    flags: i64,
    file_class: Option<ClassId>,
    info_class: Option<ClassId>,
    dir: Option<Open>,
    entry: Vec<u8>,
    index: i64,
    sub_path: Option<Vec<u8>>,
    stream: Option<Open>,
    open_mode: Vec<u8>,
    orig_path: Option<Vec<u8>>,
    line: Option<Vec<u8>>,
    csv: Option<Owner<Value>>,
    line_num: i64,
    max_line_len: i64,
    delimiter: u8,
    enclosure: u8,
    escape: Option<u8>,
    escape_set: bool,
}

/// The directory or the file an object opened, `u.dir.dirp` or
/// `u.file.stream`: the object's owner of its resource, and the only way
/// to let go of it is [`Open::close`], so a close that keeps the resource
/// listed cannot be written.
struct Open(Owner<Stream>);

impl Open {
    /// The stream, for an operation on it.
    fn stream(&self) -> Stream {
        *self.0
    }

    /// `php_stream_close`: the stream closed, then the object's owner of
    /// its resource given up, [`OwnedStream::free`].
    fn close(self, req: &mut Request) {
        self.0.free(req);
    }
}

/// What `FilesystemIterator::current()` answers by its flags: the path
/// name, an `SplFileInfo`, or the iterator itself.
type FsCurrent = Union<
    Owner<PhpString>,
    Union<Owner<Instance<SplFileInfo>>, Owner<Instance<FilesystemIterator>>>,
>;

/// What `SplFileObject::fscanf` answers, the inner `sscanf`'s.
type ScanAnswer = Owner<Raw<Option<Union<PhpArray, i64>>>>;

/// What `SplFileObject::current` answers: the line, the row, or `false`.
type FileLine = OrFalse<Union<Owner<PhpString>, Owner<PhpArray>>>;

/// `spl_other_handler` with the `oth` it serves (`spl_directory.h`): what
/// a class extending one of these lays on the object, as phar lays its
/// open archive on a `Phar`, freed with the object (`dtor`) and copied
/// with a clone (`clone`). Exported to ext/phar (design §4.10, the `spl`
/// → `phar` row).
pub trait Other: std::any::Any + Send {
    /// `oth_handler->dtor`, served as the object is freed, before
    /// anything else of it.
    fn dtor(self: Box<Self>, req: &mut Request);
    /// What a clone holds: `intern->oth = source->oth`, then
    /// `oth_handler->clone` where the class set one.
    fn clone_other(&self, req: &mut Request) -> Box<dyn Other>;
}

/// `intern->oth` and `intern->oth_handler` set on a filesystem object,
/// as `Phar::__construct` sets them; `false` for an object without the
/// payload.
pub fn set_other(req: &mut Request, object: PhpObject, other: Box<dyn Other>) -> bool {
    match object.native_mut::<Fs>(req) {
        Some(it) => {
            it.oth = Some(other);
            true
        }
        None => false,
    }
}

/// `intern->oth = NULL`: what the object holds taken off it, as
/// `PharFileInfo::__destruct` lets its entry go.
pub fn take_other(req: &mut Request, object: PhpObject) -> Option<Box<dyn Other>> {
    object.native_mut::<Fs>(req)?.oth.take()
}

/// `intern->oth` as the class that laid it down reads it: `None` where
/// the object has none or one of another type.
pub fn other_mut<T: Other>(req: &mut Request, object: PhpObject) -> Option<&mut T> {
    let other: &mut dyn Other = object.native_mut::<Fs>(req)?.oth.as_deref_mut()?;
    (other as &mut dyn std::any::Any).downcast_mut::<T>()
}

/// `intern->info_class`: the class `getFileInfo()`, `current()` and
/// phar's `offsetGet()` make, `None` for `SplFileInfo`.
pub fn info_class_of(req: &mut Request, object: PhpObject) -> Option<ClassId> {
    object.native_mut::<Fs>(req)?.info_class
}

/// `intern->info_class = class`, as `Phar::__construct` sets
/// `PharFileInfo`.
pub fn set_info_class(req: &mut Request, object: PhpObject, class: ClassId) {
    if let Some(it) = object.native_mut::<Fs>(req) {
        it.info_class = Some(class);
    }
}

/// What `phar_build` reads of an `SplFileInfo` its iterator hands it:
/// `intern->type`, and a directory's `spl_filesystem_object_get_path`
/// with its entry's `d_name`, or an info's and a file's `file_name`.
pub enum Filesystem {
    Dir {
        path: Option<Vec<u8>>,
        entry: Vec<u8>,
    },
    File {
        file_name: Option<Vec<u8>>,
    },
}

/// The [`Filesystem`] of an object, `None` for one without the payload.
pub fn filesystem(req: &mut Request, object: PhpObject) -> Option<Filesystem> {
    let path = object_path(req, object).ok()?;
    let it = object.native_mut::<Fs>(req)?;
    Some(match it.kind {
        Type::Dir => Filesystem::Dir {
            path,
            entry: it.entry.clone(),
        },
        Type::Info | Type::File => Filesystem::File {
            file_name: it.file_name.clone(),
        },
    })
}

/// `spl_filesystem_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// `spl_filesystem_object_check_handlers`, `GlobIterator`'s,
/// `SplFileObject`'s and `SplTempFileObject`'s: the same slots with
/// `clone_obj` null, so the three are uncloneable as php's are, and a
/// `get_method` that refuses every call on an object whose constructor
/// never ran.
pub(crate) struct FileHandlers;

pub(crate) static FILE_HANDLERS: FileHandlers = FileHandlers;

/// The seven classes onto `builder`, in the order
/// `PHP_MINIT_FUNCTION(spl_directory)` registers them.
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
pub(crate) fn register(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder
        .class::<SplFileInfo>()
        .class::<DirectoryIterator>()
        .class::<FilesystemIterator>()
        .class::<RecursiveDirectoryIterator>()
        .class::<GlobIterator>()
        .class::<SplFileObject>()
        .class::<SplTempFileObject>()
}

/// `class SplFileInfo implements Stringable`: a path, and what a stat
/// of it says.
///
/// ```php
/// $f = __DIR__ . '/spl-info.txt'; file_put_contents($f, 'abcd'); $i = new SplFileInfo($f);
/// var_dump($i->getPath() === __DIR__, $i->getFilename(), $i->getExtension(), $i->getBasename('.txt'), $i->getPathname() === $f, (string) $i === $f);
/// var_dump($i->getPerms() === fileperms($f), $i->getInode() === fileinode($f), $i->getSize(), $i->getOwner() === fileowner($f), $i->getGroup() === filegroup($f));
/// var_dump($i->getATime() === fileatime($f), $i->getMTime() === filemtime($f), $i->getCTime() === filectime($f), $i->getType());
/// var_dump($i->isWritable(), $i->isReadable(), $i->isExecutable(), $i->isFile(), $i->isDir(), $i->isLink(), $i->getRealPath() === realpath($f));
/// try { $i->getLinkTarget(); } catch (RuntimeException $e) { echo "no link\n"; }
/// $i->setFileClass(); $i->setInfoClass();
/// var_dump(get_class($i->getFileInfo()), $i->getPathInfo()->getFilename() === basename(__DIR__), get_class($i->openFile()), count($i->__debugInfo()));
/// try { $i->_bad_state_ex(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(12) "spl-info.txt"
/// string(3) "txt"
/// string(8) "spl-info"
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// int(4)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// string(4) "file"
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// no link
/// string(11) "SplFileInfo"
/// bool(true)
/// string(13) "SplFileObject"
/// int(2)
///
/// Deprecated: Method SplFileInfo::_bad_state_ex() is deprecated since 8.2 in %s on line 10
/// The parent constructor was not called: the object is in an invalid state
/// ```
///
/// The class is `@not-serializable`, and so is every class that extends
/// it.
///
/// ```php
/// foreach ([new SplFileInfo(__FILE__), new SplTempFileObject] as $o) {
///     try { serialize($o); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// }
/// try { unserialize('O:11:"SplFileInfo":0:{}'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Serialization of 'SplFileInfo' is not allowed
/// Serialization of 'SplTempFileObject' is not allowed
/// Unserialization of 'SplFileInfo' is not allowed
/// ```
///
/// `zend_object_alloc(sizeof(spl_filesystem_object))`, which
/// `memory_get_usage()` counts (design §5.13):
///
/// ```php
/// foreach ([false, true] as $shown) foreach (['SplFileInfo', 'DirectoryIterator', 'SplFileObject', 'SplTempFileObject', 'GlobIterator'] as $c) {
///     $r = new ReflectionClass($c);
///     $m = memory_get_usage();
///     $o = $r->newInstanceWithoutConstructor();
///     $used = memory_get_usage() - $m;
///     $o = null;
///     if ($shown) echo $c, ' ', $used, "\n";
/// }
/// ```
/// ```expect
/// SplFileInfo 1280
/// DirectoryIterator 1280
/// SplFileObject 1280
/// SplTempFileObject 1280
/// GlobIterator 1280
/// ```
#[php_class(implements(Stringable), not_serializable, size = 1208, payload = Fs, handlers = HANDLERS)]
pub(crate) struct SplFileInfo;

#[php_class]
impl SplFileInfo {
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
    fn __construct(req: &mut Request, this: This<Self>, filename: PathArg) -> Result<(), Thrown> {
        set_filename(req, this.object(), filename.0.as_bytes())
    }

    /// `getPath(): string`.
    #[php(tentative)]
    fn getPath(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let path = object_path(req, this.object())?.unwrap_or_default();
        PhpString::new(req, &path)
    }

    /// `getFilename(): string`.
    #[php(tentative)]
    fn getFilename(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let name = short_name(req, this.object())?;
        PhpString::new(req, &name)
    }

    /// `getExtension(): string`.
    #[php(tentative)]
    fn getExtension(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let name = short_name(req, this.object())?;
        PhpString::new(req, &extension(&name))
    }

    /// `getBasename(string $suffix = ""): string`.
    #[php(tentative)]
    fn getBasename(
        req: &mut Request,
        this: This<Self>,
        #[php(default = "")] suffix: PhpString,
    ) -> Result<Owner<PhpString>, Thrown> {
        let name = short_name(req, this.object())?;
        PhpString::new(req, &basename(&name, suffix.as_bytes()))
    }

    /// `getPathname(): string`, and `__toString()`.
    #[php(tentative)]
    fn getPathname(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let name = pathname(req, this.object())?.unwrap_or_default();
        PhpString::new(req, &name)
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
    #[php(tentative)]
    fn getPerms(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        stat_field(req, this.object(), 2, "getPerms")
    }

    /// `getInode(): int|false`.
    #[php(tentative)]
    fn getInode(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        stat_field(req, this.object(), 1, "getInode")
    }

    /// `getSize(): int|false`.
    #[php(tentative)]
    fn getSize(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        stat_field(req, this.object(), 7, "getSize")
    }

    /// `getOwner(): int|false`.
    #[php(tentative)]
    fn getOwner(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        stat_field(req, this.object(), 4, "getOwner")
    }

    /// `getGroup(): int|false`.
    #[php(tentative)]
    fn getGroup(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        stat_field(req, this.object(), 5, "getGroup")
    }

    /// `getATime(): int|false`.
    #[php(tentative)]
    fn getATime(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        stat_field(req, this.object(), 8, "getATime")
    }

    /// `getMTime(): int|false`.
    #[php(tentative)]
    fn getMTime(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        stat_field(req, this.object(), 9, "getMTime")
    }

    /// `getCTime(): int|false`.
    #[php(tentative)]
    fn getCTime(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        stat_field(req, this.object(), 10, "getCTime")
    }

    /// `getType(): string|false`, of the link itself for a link.
    #[php(tentative)]
    fn getType(req: &mut Request, this: This<Self>) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let this = this.object();
        if let Some((_, s)) = stat(req, this, true)? {
            return Ok(OrFalse::Value(PhpString::new(req, kind_name(s[2]))?));
        }
        if unnamed(req, this)? {
            return Ok(OrFalse::False);
        }
        let name = String::from_utf8_lossy(&file_name(req, this)?).into_owned();
        Err(throw(
            req,
            "RuntimeException",
            &format!("SplFileInfo::getType(): Lstat failed for {name}"),
        ))
    }

    /// `isWritable(): bool`.
    #[php(tentative)]
    fn isWritable(req: &mut Request, this: This<Self>) -> Result<Owner<Raw<bool>>, Thrown> {
        access(req, this.object(), php_ext_standard::Fs::IsW)
    }

    /// `isReadable(): bool`.
    #[php(tentative)]
    fn isReadable(req: &mut Request, this: This<Self>) -> Result<Owner<Raw<bool>>, Thrown> {
        access(req, this.object(), php_ext_standard::Fs::IsR)
    }

    /// `isExecutable(): bool`.
    #[php(tentative)]
    fn isExecutable(req: &mut Request, this: This<Self>) -> Result<Owner<Raw<bool>>, Thrown> {
        access(req, this.object(), php_ext_standard::Fs::IsX)
    }

    /// `isFile(): bool`.
    #[php(tentative)]
    fn isFile(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        is_kind(req, this.object(), false, 0o100_000)
    }

    /// `isDir(): bool`.
    #[php(tentative)]
    fn isDir(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        is_kind(req, this.object(), false, 0o040_000)
    }

    /// `isLink(): bool`.
    #[php(tentative)]
    fn isLink(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        is_kind(req, this.object(), true, 0o120_000)
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
    #[php(tentative)]
    fn getLinkTarget(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let name = file_name(req, this.object())?;
        let host = req.environment().absolutize(&name);
        match std::fs::read_link(&host) {
            Ok(target) => Ok(OrFalse::Value(PhpString::new(
                req,
                target.as_os_str().as_encoded_bytes(),
            )?)),
            Err(error) => {
                let text = error.to_string();
                let text = text
                    .split_once(" (os error")
                    .map_or(text.as_str(), |(t, _)| t)
                    .to_owned();
                let shown = String::from_utf8_lossy(&name).into_owned();
                Err(throw_exception_ex(
                    req,
                    "RuntimeException",
                    &format!("Unable to read link {shown}, error: {text}"),
                ))
            }
        }
    }

    /// `getRealPath(): string|false`.
    ///
    /// A `GlobIterator` that matched nothing stands on no entry: it has
    /// no name, so its real path is `false`, until a call names it `""`,
    /// which every stat answers `false` for quietly (`php_stat`), and
    /// whose real path is the working directory's.
    ///
    /// ```php
    /// $o = new GlobIterator(__DIR__ . "/*.no-such-extension");
    /// var_dump($o->getRealPath(), $o->getPathname(), $o->getRealPath());
    /// var_dump($o->getATime(), $o->getType(), $o->getSize(), $o->getFilename(), $o->getRealPath() === getcwd(), $o->count());
    /// echo str_replace(__DIR__, "", var_export($o->__debugInfo()["\0DirectoryIterator\0glob"], true)), "\n";
    /// ```
    /// ```expect
    /// bool(false)
    /// string(0) ""
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// string(0) ""
    /// bool(true)
    /// int(0)
    /// 'glob:///*.no-such-extension'
    /// ```
    #[php(tentative)]
    fn getRealPath(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let this = this.object();
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
        // `intern->file_name` as it stands: a directory on no entry has
        // none until a call that names it, and then `""`, php's
        // `VCWD_REALPATH("")`, the working directory.
        let name = match orig {
            Some(orig) => Some(orig),
            None => fs(req, this, |it| it.file_name.clone())?,
        };
        let name = name.map(|n| if n.is_empty() { b".".to_vec() } else { n });
        match name.and_then(|n| req.environment_mut().realpath(&n)) {
            Some(real) => Ok(OrFalse::Value(PhpString::new(
                req,
                real.as_os_str().as_encoded_bytes(),
            )?)),
            None => Ok(OrFalse::False),
        }
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
    #[php(tentative)]
    fn getFileInfo(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] class: Raw<Option<PhpString>>,
    ) -> Result<Owner<Instance<SplFileInfo>>, Thrown> {
        let this = this.object();
        let base = info_base(req, this)?;
        let chosen = class_argument(req, class.value(), 1, &base, true)?;
        let chosen = match chosen {
            Some(c) => Some(c),
            None => fs(req, this, |it| it.info_class)?,
        };
        let class = info_class(req, chosen)?;
        let (kind, entry) = fs(req, this, |it| (it.kind, it.entry.is_empty()))?;
        if kind == Type::Dir && entry {
            return Err(throw_exception_ex(
                req,
                "RuntimeException",
                "Could not open file",
            ));
        }
        let name = file_name(req, this)?;
        create_info(req, &name, class).map(Owner::into_instance)
    }

    /// `getPathInfo(?string $class = null): ?SplFileInfo`; the witness is
    /// `getFileInfo`'s and the class's.
    #[php(tentative)]
    fn getPathInfo(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] class: Raw<Option<PhpString>>,
    ) -> Result<Option<Owner<Instance<SplFileInfo>>>, Thrown> {
        let this = this.object();
        let chosen = class_argument(req, class.value(), 1, b"SplFileInfo", true)?;
        let chosen = match chosen {
            Some(c) => Some(c),
            None => fs(req, this, |it| it.info_class)?,
        };
        let class = info_class(req, chosen)?;
        match pathname(req, this)? {
            Some(path) if !path.is_empty() => {
                create_info(req, &dirname(&path), class).map(|o| Some(o.into_instance()))
            }
            _ => Ok(None),
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
    /// try { (new SplFileInfo($f))->openFile('r', false, []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(13) "SplFileObject"
    /// string(2) "x
    /// "
    /// Cannot use SplFileObject with directories
    /// SplFileInfo::openFile(): Argument #3 ($context) must be of type resource or null, array given
    /// ```
    #[php(tentative)]
    fn openFile(
        req: &mut Request,
        this: This<Self>,
        #[php(default = "r")] mode: PhpString,
        #[php(default = false)] useIncludePath: bool,
        #[php(default = null)] context: Option<Resource>,
    ) -> Result<Owner<Instance<SplFileObject>>, Thrown> {
        let _ = context;
        let this = this.object();
        let chosen = fs(req, this, |it| it.file_class)?;
        let class = match chosen {
            Some(class) => class,
            None => linked(req, b"SplFileObject")?,
        };
        let (kind, entry) = fs(req, this, |it| (it.kind, it.entry.is_empty()))?;
        if kind == Type::Dir && entry {
            return Err(throw_exception_ex(
                req,
                "RuntimeException",
                "Could not open file",
            ));
        }
        let name = file_name(req, this)?;
        let name_value = crate::string_value(req, &name)?;
        let mode_value = Owner::take(req, Value::string(mode));
        let opened = instantiate(
            req,
            class,
            &[*name_value, *mode_value, Value::bool(useIncludePath)],
        );
        name_value.give_up(req);
        mode_value.give_up(req);
        opened.map(Owner::into_instance)
    }

    /// `setFileClass(string $class = SplFileObject::class): void`: an
    /// omitted class is the default, evaluated to its name.
    ///
    /// ```php
    /// class F extends SplFileObject {}
    /// $i = new SplFileInfo(__FILE__);
    /// $i->setFileClass('F');
    /// $i->setFileClass();
    /// $i->setInfoClass();
    /// echo get_class($i->openFile()), ' ', get_class($i->getFileInfo()), "\n";
    /// ```
    /// ```expect
    /// SplFileObject SplFileInfo
    /// ```
    #[php(tentative)]
    fn setFileClass(
        req: &mut Request,
        this: This<Self>,
        #[php(default = SplFileObject::class)] class: Raw<PhpString>,
    ) -> Result<(), Thrown> {
        let class = class_argument(req, class.value(), 1, b"SplFileObject", false)?;
        fs(req, this.object(), |it| it.file_class = class)
    }

    /// `setInfoClass(string $class = SplFileInfo::class): void`; the
    /// witness is `getFileInfo`'s and the class's.
    ///
    /// The class named is `zend_lookup_class`, which asks the autoloaders.
    ///
    /// ```php
    /// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'Info') { class Info extends SplFileInfo {} } });
    /// $f = new SplFileInfo('x');
    /// $f->setInfoClass('Info');
    /// echo get_class($f->getFileInfo()), "\n";
    /// ```
    /// ```expect
    /// load Info
    /// Info
    /// ```
    #[php(tentative)]
    fn setInfoClass(
        req: &mut Request,
        this: This<Self>,
        #[php(default = SplFileInfo::class)] class: Raw<PhpString>,
    ) -> Result<(), Thrown> {
        let class = class_argument(req, class.value(), 1, b"SplFileInfo", false)?;
        fs(req, this.object(), |it| it.info_class = class)
    }

    #[php(implementation = SplFileInfo::getPathname)]
    fn __toString(this: This<Self>) -> PhpString;

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
    #[php(tentative)]
    fn __debugInfo(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        debug_info(req, this.object())
    }

    /// `_bad_state_ex(): void`, deprecated, which the call raises from the
    /// declaration before this runs: php's `Error`.
    ///
    /// ```php
    /// try { (new SplFileInfo('x'))->_bad_state_ex(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Deprecated: Method SplFileInfo::_bad_state_ex() is deprecated since 8.2 in %s on line 2
    /// The parent constructor was not called: the object is in an invalid state
    /// ```
    #[php(final, deprecated(since = "8.2"))]
    fn _bad_state_ex(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let _ = this;
        Err(throw_error(
            req,
            "Error",
            "The parent constructor was not called: the object is in an invalid state",
        ))
    }
}

/// `class DirectoryIterator extends SplFileInfo implements
/// SeekableIterator`: a directory's entries, the iterator itself each
/// one's `SplFileInfo`.
///
/// ```php
/// $d = __DIR__ . '/spl-dir2'; @mkdir($d); touch("$d/only.txt");
/// $it = new DirectoryIterator($d);
/// $seen = []; foreach ($it as $k => $entry) { $seen[] = ($entry === $it ? 'self ' : '') . ($entry->isDot() ? 'dot ' : '') . $entry->getFilename(); }
/// sort($seen); echo implode(', ', $seen), "\n";
/// $it->rewind(); $it->seek(2); var_dump($it->key(), $it->valid());
/// try { new DirectoryIterator(__DIR__ . '/nope'); } catch (UnexpectedValueException $e) { echo str_replace(__DIR__, '', $e->getMessage()), "\n"; }
/// try { new DirectoryIterator(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// $it->rewind(); var_dump($it->getExtension() === pathinfo($it->current()->getFilename(), PATHINFO_EXTENSION), $it->getBasename() === (string) $it);
/// ```
/// ```expect
/// self dot ., self dot .., self only.txt
/// int(2)
/// bool(true)
/// DirectoryIterator::__construct(/nope): Failed to open directory: No such file or directory
/// DirectoryIterator::__construct(): Argument #1 ($directory) must not be empty
/// bool(true)
/// bool(true)
/// ```
#[php_class(extends(SplFileInfo), implements(SeekableIterator))]
pub(crate) struct DirectoryIterator;

#[php_class]
impl DirectoryIterator {
    /// `DirectoryIterator::__construct(string $directory)`: the entries as
    /// the iterator itself, keyed by index.
    fn __construct(req: &mut Request, this: This<Self>, directory: PathArg) -> Result<(), Thrown> {
        dir_construct(
            req,
            this.object(),
            directory.0.as_bytes(),
            FilesystemIterator::KEY_AS_PATHNAME | FilesystemIterator::CURRENT_AS_SELF,
        )
    }

    /// `DirectoryIterator::getFilename(): string` and `__toString()`, the
    /// entry's name.
    #[php(tentative)]
    fn getFilename(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let this = dir_checked(req, this.object())?;
        let entry = fs(req, this, |it| it.entry.clone())?;
        PhpString::new(req, &entry)
    }

    /// `DirectoryIterator::getExtension(): string`.
    #[php(tentative)]
    fn getExtension(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let this = dir_checked(req, this.object())?;
        let entry = fs(req, this, |it| it.entry.clone())?;
        PhpString::new(req, &extension(&entry))
    }

    /// `DirectoryIterator::getBasename(string $suffix = ""): string`.
    #[php(tentative)]
    fn getBasename(
        req: &mut Request,
        this: This<Self>,
        #[php(default = "")] suffix: PhpString,
    ) -> Result<Owner<PhpString>, Thrown> {
        let this = dir_checked(req, this.object())?;
        let entry = fs(req, this, |it| it.entry.clone())?;
        PhpString::new(req, &basename(&entry, suffix.as_bytes()))
    }

    /// `isDot(): bool`.
    #[php(tentative)]
    fn isDot(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = dir_checked(req, this.object())?;
        fs(req, this, |it| is_dot(&it.entry))
    }

    /// `rewind(): void`, `php_stream_rewinddir` and the first entry.
    #[php(tentative)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = dir_checked(req, this.object())?;
        let dir = fs(req, this, |it| {
            it.index = 0;
            it.dir.as_ref().map(Open::stream)
        })?;
        if let Some(dir) = dir {
            dir.seek(req, 0, Whence::Set);
        }
        dir_step(req, this)
    }

    /// `valid(): bool`.
    #[php(tentative)]
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = dir_checked(req, this.object())?;
        fs(req, this, |it| !it.entry.is_empty())
    }

    /// `DirectoryIterator::key(): mixed`, the index.
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let this = dir_checked(req, this.object())?;
        Ok(Value::int(fs(req, this, |it| it.index)?))
    }

    /// `DirectoryIterator::current(): mixed`, the iterator itself.
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let this = dir_checked(req, this.object())?;
        Ok(this.as_value())
    }

    /// `next(): void`.
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = dir_checked(req, this.object())?;
        fs(req, this, |it| it.index += 1)?;
        dir_step(req, this)
    }

    /// `seek(int $offset): void`: `rewind()`, `valid()` and `next()` served
    /// through the class, as php serves them.
    #[php(tentative)]
    fn seek(req: &mut Request, this: This<Self>, offset: i64) -> Result<(), Thrown> {
        let this = dir_checked(req, this.object())?;
        let pos = offset;
        if fs(req, this, |it| it.index)? > pos {
            crate::effect(req, this, b"rewind")?;
        }
        while fs(req, this, |it| it.index)? < pos {
            if !crate::valid(req, this)? {
                return Err(throw_exception_ex(
                    req,
                    "OutOfBoundsException",
                    &format!("Seek position {pos} is out of range"),
                ));
            }
            crate::effect(req, this, b"next")?;
        }
        Ok(())
    }

    #[php(implementation = DirectoryIterator::getFilename)]
    fn __toString(this: This<Self>) -> PhpString;
}

/// `class FilesystemIterator extends DirectoryIterator`: the entries
/// keyed and served as its flags say, the dots skipped by default.
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
#[php_class(extends(DirectoryIterator))]
pub(crate) struct FilesystemIterator;

#[php_class]
impl FilesystemIterator {
    const CURRENT_MODE_MASK: i64 = 0xF0;
    const CURRENT_AS_PATHNAME: i64 = 0x20;
    const CURRENT_AS_FILEINFO: i64 = 0;
    const CURRENT_AS_SELF: i64 = 0x10;
    const KEY_MODE_MASK: i64 = 0xF00;
    const KEY_AS_PATHNAME: i64 = 0;
    const FOLLOW_SYMLINKS: i64 = 0x4000;
    const KEY_AS_FILENAME: i64 = 0x100;
    const NEW_CURRENT_AND_KEY: i64 = Self::KEY_AS_FILENAME | Self::CURRENT_AS_FILEINFO;
    const OTHER_MODE_MASK: i64 = 0x7000;
    const SKIP_DOTS: i64 = 0x1000;
    const UNIX_PATHS: i64 = 0x2000;

    /// `FilesystemIterator::__construct(string $directory, int $flags =
    /// KEY_AS_PATHNAME | CURRENT_AS_FILEINFO | SKIP_DOTS)`; the witness is
    /// the class's.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        directory: PathArg,
        #[php(default = FilesystemIterator::KEY_AS_PATHNAME | FilesystemIterator::CURRENT_AS_FILEINFO | FilesystemIterator::SKIP_DOTS)]
        flags: i64,
    ) -> Result<(), Thrown> {
        dir_construct(req, this.object(), directory.0.as_bytes(), flags)
    }

    #[php(tentative, implementation = DirectoryIterator::rewind)]
    fn rewind(this: This<Self>);

    /// `FilesystemIterator::key(): string`, by the key flags.
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let this = this.object();
        let (flags, entry) = fs(req, this, |it| (it.flags, it.entry.clone()))?;
        if flags & Self::KEY_AS_FILENAME != 0 {
            return PhpString::new(req, &entry);
        }
        let name = file_name(req, this)?;
        PhpString::new(req, &name)
    }

    /// `FilesystemIterator::current()`, by the current flags: the path
    /// name, an `SplFileInfo`, or the iterator itself.
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<FsCurrent, Thrown> {
        let this = this.object();
        let flags = fs(req, this, |it| it.flags)?;
        if flags & Self::CURRENT_AS_PATHNAME != 0 {
            let name = file_name(req, this)?;
            return Ok(Union::A(PhpString::new(req, &name)?));
        }
        if flags & Self::CURRENT_AS_SELF != 0 {
            return Ok(Union::B(Union::B(Owner::take(req, this).into_instance())));
        }
        let name = file_name(req, this)?;
        let chosen = fs(req, this, |it| it.info_class)?;
        let class = info_class(req, chosen)?;
        let info = create_info(req, &name, class)?;
        Ok(Union::B(Union::A(info.into_instance())))
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
    #[php(tentative)]
    fn getFlags(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let mask = Self::KEY_MODE_MASK | Self::CURRENT_MODE_MASK | Self::OTHER_MODE_MASK;
        fs(req, this.object(), |it| it.flags & mask)
    }

    /// `setFlags(int $flags): void`.
    #[php(tentative)]
    fn setFlags(req: &mut Request, this: This<Self>, flags: i64) -> Result<(), Thrown> {
        let mask = Self::KEY_MODE_MASK | Self::CURRENT_MODE_MASK | Self::OTHER_MODE_MASK;
        fs(req, this.object(), |it| {
            it.flags = (it.flags & !mask) | (flags & mask);
        })
    }
}

/// `class RecursiveDirectoryIterator extends FilesystemIterator
/// implements RecursiveIterator`: each directory's entries as its
/// children.
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
#[php_class(extends(FilesystemIterator), implements(RecursiveIterator))]
pub(crate) struct RecursiveDirectoryIterator;

#[php_class]
impl RecursiveDirectoryIterator {
    /// `RecursiveDirectoryIterator::__construct(string $directory, int
    /// $flags = KEY_AS_PATHNAME | CURRENT_AS_FILEINFO)`.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        directory: PathArg,
        #[php(default = FilesystemIterator::KEY_AS_PATHNAME | FilesystemIterator::CURRENT_AS_FILEINFO)]
        flags: i64,
    ) -> Result<(), Thrown> {
        dir_construct(req, this.object(), directory.0.as_bytes(), flags)
    }

    /// `hasChildren(bool $allowLinks = false): bool`: a directory that is no
    /// dot, a link only where allowed or `FOLLOW_SYMLINKS` is set.
    #[php(tentative)]
    fn hasChildren(
        req: &mut Request,
        this: This<Self>,
        #[php(default = false)] allowLinks: bool,
    ) -> Result<bool, Thrown> {
        let this = this.object();
        let (entry, flags) = fs(req, this, |it| (it.entry.clone(), it.flags))?;
        if entry.is_empty() || is_dot(&entry) {
            return Ok(false);
        }
        let name = file_name(req, this)?;
        let Some(lstat) = req.url_stat(&name, true, true)?.found() else {
            return Ok(false);
        };
        if lstat[2] & 0o170_000 != 0o120_000 {
            return Ok(lstat[2] & 0o170_000 == 0o040_000);
        }
        if !allowLinks && flags & FilesystemIterator::FOLLOW_SYMLINKS == 0 {
            return Ok(false);
        }
        let stat = req.url_stat(&name, false, true)?.found();
        Ok(stat.is_some_and(|s| s[2] & 0o170_000 == 0o040_000))
    }

    /// `getChildren(): RecursiveDirectoryIterator`: a new iterator of this
    /// class over the entry, its sub path one level on.
    #[php(tentative)]
    fn getChildren(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<RecursiveDirectoryIterator>>, Thrown> {
        let this = this.object();
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
        let made = instantiate(req, this.class(), &[*arg, Value::int(flags)]);
        arg.give_up(req);
        let child = made?;
        let sub_path = match sub {
            Some(sub) if !sub.is_empty() => [sub.as_slice(), b"/", entry.as_slice()].concat(),
            _ => entry,
        };
        fs(req, *child, |it| {
            it.sub_path = Some(sub_path);
            it.info_class = info;
            it.file_class = file;
        })?;
        Ok(child.into_instance())
    }

    /// `getSubPath(): string`.
    #[php(tentative)]
    fn getSubPath(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let sub = fs(req, this.object(), |it| it.sub_path.clone())?.unwrap_or_default();
        PhpString::new(req, &sub)
    }

    /// `getSubPathname(): string`.
    #[php(tentative)]
    fn getSubPathname(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let (sub, entry) = fs(req, this.object(), |it| {
            (it.sub_path.clone(), it.entry.clone())
        })?;
        let out = match sub {
            Some(sub) => [sub.as_slice(), b"/", entry.as_slice()].concat(),
            None => entry,
        };
        PhpString::new(req, &out)
    }
}

/// `class GlobIterator extends FilesystemIterator implements Countable`:
/// the paths a pattern matches.
///
/// ```php
/// $d = __DIR__ . '/spl-glob'; @mkdir($d); touch("$d/a.txt"); touch("$d/b.txt"); touch("$d/c.log");
/// $g = new GlobIterator("$d/*.txt");
/// var_dump(count($g), $g->count());
/// foreach ($g as $k => $v) echo str_replace($d, '', $k), ' ', $v->getFilename(), "\n";
/// ```
/// ```expect
/// int(2)
/// int(2)
/// /a.txt a.txt
/// /b.txt b.txt
/// ```
///
/// The path is the glob stream's, the directory of the match it read
/// last (`php_glob_stream_get_path`), none for a match with no `/`; the
/// count is the stream's; and the stream is a resource `fclose` refuses.
///
/// ```php
/// $d = __DIR__ . '/spl-glob-path'; @mkdir($d); @mkdir("$d/s"); touch("$d/a.51068"); touch("$d/s/b.51068");
/// chdir($d);
/// foreach (new DirectoryIterator('glob://*.51068') as $f) var_dump($f->getFilename(), $f->getPath(), $f->getSize());
/// $g = new GlobIterator('*/*.51068');
/// foreach ($g as $k => $f) var_dump($k, $g->getPath(), $f->getPathname());
/// var_dump($g->getPath(), count($g));
/// $r = get_resources(); fclose(end($r));
/// unlink("$d/s/b.51068"); rmdir("$d/s"); unlink("$d/a.51068"); rmdir($d);
/// ```
/// ```expect
/// string(7) "a.51068"
/// string(0) ""
/// int(0)
/// string(9) "s/b.51068"
/// string(1) "s"
/// string(9) "s/b.51068"
/// string(0) ""
/// int(1)
///
/// Warning: fclose(): cannot close the provided stream, as it must not be manually closed in %s on line 8
/// ```
#[php_class(
    extends(FilesystemIterator),
    implements(Countable),
    size = 1208, payload = Fs,
    handlers = FILE_HANDLERS
)]
pub(crate) struct GlobIterator;

#[php_class]
impl GlobIterator {
    /// `GlobIterator::__construct(string $pattern, int $flags =
    /// KEY_AS_PATHNAME | CURRENT_AS_FILEINFO)`: the pattern opened as a
    /// `glob://` directory, `glob://` put before it where it has none
    /// (`DIT_CTOR_GLOB`).
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        pattern: PathArg,
        #[php(default = FilesystemIterator::KEY_AS_PATHNAME | FilesystemIterator::CURRENT_AS_FILEINFO)]
        flags: i64,
    ) -> Result<(), Thrown> {
        let pattern = pattern.0.as_bytes();
        let path = if pattern.is_empty() || pattern.starts_with(b"glob://") {
            pattern.to_vec()
        } else {
            [b"glob://".as_slice(), pattern].concat()
        };
        dir_construct(req, this.object(), &path, flags)
    }

    /// `GlobIterator::count(): int`.
    #[php(tentative)]
    fn count(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        match glob_stream(req, this.object())?.and_then(|dir| dir.glob_count(req)) {
            Some(n) => Ok(i64::try_from(n).unwrap_or(i64::MAX)),
            None => Err(throw_error(req, "Error", "GlobIterator is not initialized")),
        }
    }
}

/// `class SplFileObject extends SplFileInfo implements
/// RecursiveIterator, SeekableIterator`: a file's lines, over the stream
/// `fopen` opens, uncloneable (`spl_filesystem_object_check_handlers`).
///
/// ```php
/// $i = clone new SplFileInfo(__FILE__);
/// echo get_class($i), "\n";
/// try { clone new SplTempFileObject(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $t = new SplTempFileObject();
/// var_dump($t->fwrite("ab\ncd\n"), $t->fputcsv(['x', 'y'], ',', '"', ''), $t->fflush(), $t->ftell(), $t->flock(LOCK_SH), $t->fseek(0));
/// var_dump($t->fgetc(), $t->eof(), $t->valid(), (string) $t, $t->key());
/// $t->rewind(); var_dump($t->current()); $t->next(); var_dump($t->key(), $t->fpassthru());
/// $t->setFlags(SplFileObject::READ_CSV); var_dump($t->getFlags(), $t->getMaxLineLen(), $t->hasChildren(), $t->getChildren(), $t->getCsvControl()[0]);
/// ```
/// ```expect
/// SplFileInfo
/// Trying to clone an uncloneable object of class SplTempFileObject
/// int(6)
/// int(4)
/// bool(true)
/// int(10)
/// bool(false)
/// int(0)
/// string(1) "a"
/// bool(false)
/// bool(true)
/// string(2) "b
/// "
/// int(0)
/// string(3) "ab
/// "
/// cd
/// x,y
/// int(1)
/// int(7)
/// int(8)
/// int(0)
/// bool(false)
/// NULL
/// string(1) ","
/// ```
#[php_class(
    extends(SplFileInfo),
    implements(RecursiveIterator, SeekableIterator),
    size = 1208, payload = Fs,
    handlers = FILE_HANDLERS
)]
pub(crate) struct SplFileObject;

#[php_class]
impl SplFileObject {
    const DROP_NEW_LINE: i64 = 1;
    const READ_AHEAD: i64 = 2;
    const SKIP_EMPTY: i64 = 4;
    const READ_CSV: i64 = 8;

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
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        filename: PathArg,
        #[php(default = "r")] mode: PhpString,
        #[php(default = false)] useIncludePath: bool,
        #[php(default = null)] context: Option<Resource>,
    ) -> Result<(), Thrown> {
        let _ = context;
        let this = this.object();
        let name = filename.0.as_bytes().to_vec();
        if fs(req, this, |it| it.stream.is_some())? {
            return Err(throw_error(req, "Error", "Cannot call constructor twice"));
        }
        file_open(req, this, &name, mode.as_bytes(), useIncludePath)?;
        // The path is the directory of the path the stream opened.
        let orig = fs(req, this, |it| it.orig_path.clone())?.unwrap_or(name);
        let mut len = orig.len();
        if len > 1 && slash(orig[len - 1]) {
            len -= 1;
        }
        while len > 1 && !slash(orig[len - 1]) {
            len -= 1;
        }
        len = len.saturating_sub(1);
        let path = orig[..len].to_vec();
        fs(req, this, |it| it.path = Some(path))
    }

    /// `rewind(): void`.
    #[php(tentative)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        file_rewind_to_start(req, this.object())
    }

    /// `eof(): bool`.
    #[php(tentative)]
    fn eof(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, stream) = file_checked(req, this.object())?;
        Ok(stream.eof(req))
    }

    /// `valid(): bool`: a line read ahead under `READ_AHEAD`, and not at
    /// the end otherwise.
    #[php(tentative)]
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (flags, has, stream) = fs(req, this.object(), |it| {
            (
                it.flags,
                it.line.is_some() || it.csv.is_some(),
                it.stream.as_ref().map(Open::stream),
            )
        })?;
        if flags & Self::READ_AHEAD != 0 {
            return Ok(has);
        }
        Ok(stream.is_some_and(|s| !s.eof(req)))
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
    #[php(tentative)]
    fn fgets(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let (this, _) = file_checked(req, this.object())?;
        read_line_raw(req, this, false, 1, false)?;
        let line = fs(req, this, |it| it.line.clone())?.unwrap_or_default();
        PhpString::new(req, &line)
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
    #[php(tentative)]
    fn fread(
        req: &mut Request,
        this: This<Self>,
        length: i64,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let (_, stream) = file_checked(req, this.object())?;
        if length <= 0 {
            return Err(arg_value_error(req, 1, "be greater than 0"));
        }
        let len = usize::try_from(length).unwrap_or(usize::MAX);
        Ok(match stream.read(req, len)? {
            Some(s) => OrFalse::Value(s),
            None => OrFalse::False,
        })
    }

    /// `fgetcsv(string $separator = ",", string $enclosure = "\"", string
    /// $escape = "\\"): array|false`, through `ext/standard`'s `php_fgetcsv`,
    /// a field whose enclosure is open at the end of the line read on
    /// into the next; each control the object's own where the call leaves
    /// it out.
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
    #[php(tentative)]
    fn fgetcsv(
        req: &mut Request,
        this: This<Self>,
        #[php(default = ",")] separator: Passed<PhpString>,
        #[php(default = "\"")] enclosure: Passed<PhpString>,
        #[php(default = "\\")] escape: Passed<PhpString>,
    ) -> Result<OrFalse<PhpArray>, Thrown> {
        let (this, _) = file_checked(req, this.object())?;
        let (d, e) = fs(req, this, |it| (it.delimiter, it.enclosure))?;
        let delimiter = control_char(req, separator, 1, d)?;
        let enclosure = control_char(req, enclosure, 2, e)?;
        let escape = escape_arg(req, escape, 3, this)?;
        if !read_csv(req, this, true, (delimiter, enclosure, escape))? {
            return Ok(OrFalse::False);
        }
        let csv = fs(req, this, |it| it.csv.as_deref().copied())?;
        Ok(match csv.and_then(|c| c.as_array()) {
            Some(csv) => OrFalse::Value(csv),
            None => OrFalse::False,
        })
    }

    /// `fputcsv(array $fields, ...): int|false`, through `ext/standard`'s
    /// `php_fputcsv` on the stream, called directly as php's method calls
    /// it; each control the object's own where the call leaves it out.
    #[php(tentative)]
    fn fputcsv(
        req: &mut Request,
        this: This<Self>,
        fields: PhpArray,
        #[php(default = ",")] separator: Passed<PhpString>,
        #[php(default = "\"")] enclosure: Passed<PhpString>,
        #[php(default = "\\")] escape: Passed<PhpString>,
        #[php(default = "\n")] eol: Passed<PhpString>,
    ) -> Result<OrFalse<i64>, Thrown> {
        let (this, stream) = file_checked(req, this.object())?;
        let (d, e) = fs(req, this, |it| (it.delimiter, it.enclosure))?;
        let delimiter = control_char(req, separator, 2, d)?;
        let enclosure = control_char(req, enclosure, 3, e)?;
        let escape = escape_arg(req, escape, 4, this)?;
        let eol = match eol {
            Passed::Given(eol) => Some(eol),
            Passed::Omitted => None,
        };
        let eol = eol.as_ref().map(PhpString::as_bytes);
        let control = (delimiter, enclosure, escape);
        let written = php_ext_standard::php_fputcsv(req, stream, fields, control, eol)?;
        Ok(written.map_or(OrFalse::False, |n| {
            OrFalse::Value(i64::try_from(n).unwrap_or(i64::MAX))
        }))
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
    #[php(tentative)]
    fn setCsvControl(
        req: &mut Request,
        this: This<Self>,
        #[php(default = ",")] separator: Passed<PhpString>,
        #[php(default = "\"")] enclosure: Passed<PhpString>,
        #[php(default = "\\")] escape: Passed<PhpString>,
    ) -> Result<(), Thrown> {
        let delimiter = control_char(req, separator, 1, b',')?;
        let enclosure = control_char(req, enclosure, 2, b'"')?;
        let (escape, set) = match escape {
            Passed::Given(given) => match given.as_bytes() {
                [] => (None, true),
                [one] => (Some(*one), true),
                _ => return Err(arg_value_error(req, 3, "be empty or a single character")),
            },
            Passed::Omitted => {
                // `php_csv_handle_escape_argument(NULL, 3)`.
                php_ext::raising::docref(
                    req,
                    php_ext::raising::Level::Deprecated,
                    "the $escape parameter must be provided as its default value will change",
                );
                php_ext::raising::pending(req)?;
                (Some(b'\\'), false)
            }
        };
        fs(req, this.object(), |it| {
            it.delimiter = delimiter;
            it.enclosure = enclosure;
            it.escape = escape;
            if set {
                it.escape_set = true;
            }
        })
    }

    /// `getCsvControl(): array`; the witness is `setCsvControl`'s and the
    /// class's.
    #[php(tentative)]
    fn getCsvControl(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let (d, e, x) = fs(req, this.object(), |it| {
            (it.delimiter, it.enclosure, it.escape)
        })?;
        let mut out = PhpArray::new(req)?;
        for bytes in [vec![d], vec![e], x.map(|x| vec![x]).unwrap_or_default()] {
            let value = crate::string_value(req, &bytes)?;
            out.append(req, value)?;
        }
        Ok(out)
    }

    /// `flock(int $operation, &$wouldBlock = null): bool`.
    #[php(tentative)]
    fn flock(
        req: &mut Request,
        this: This<Self>,
        operation: i64,
        #[php(default = null)] wouldBlock: Ref<Untyped<Value>>,
    ) -> Result<bool, Thrown> {
        let _ = wouldBlock;
        let (_, stream) = file_checked(req, this.object())?;
        if !(1..=3).contains(&(operation & 3)) {
            return Err(arg_value_error(
                req,
                1,
                "be one of LOCK_SH, LOCK_EX, or LOCK_UN",
            ));
        }
        let native = [0, 1, 2, 8][usize::try_from(operation & 3).unwrap_or(0)] | (operation & 4);
        Ok(stream.flock(req, native))
    }

    /// `fflush(): bool`.
    #[php(tentative)]
    fn fflush(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, stream) = file_checked(req, this.object())?;
        Ok(stream.flush(req))
    }

    /// `ftell(): int|false`.
    #[php(tentative)]
    fn ftell(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        let (_, stream) = file_checked(req, this.object())?;
        let at = stream.tell(req);
        Ok(if at == -1 {
            OrFalse::False
        } else {
            OrFalse::Value(at)
        })
    }

    /// `fseek(int $offset, int $whence = SEEK_SET): int`, `0` or `-1`.
    #[php(tentative)]
    fn fseek(
        req: &mut Request,
        this: This<Self>,
        offset: i64,
        #[php(default = SEEK_SET)] whence: i64,
    ) -> Result<i64, Thrown> {
        let (this, stream) = file_checked(req, this.object())?;
        free_line(req, this)?;
        Ok(if stream.seek(req, offset, Whence::from_int(whence)) {
            0
        } else {
            -1
        })
    }

    /// `fgetc(): string|false`: one byte, the line count moved on a `\n`.
    #[php(tentative)]
    fn fgetc(req: &mut Request, this: This<Self>) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let (this, stream) = file_checked(req, this.object())?;
        free_line(req, this)?;
        match stream.read(req, 1)? {
            Some(s) if !s.is_empty() => {
                if s.as_bytes() == b"\n" {
                    fs(req, this, |it| it.line_num += 1)?;
                }
                Ok(OrFalse::Value(s))
            }
            Some(s) => {
                s.give_up(req);
                Ok(OrFalse::False)
            }
            None => Ok(OrFalse::False),
        }
    }

    /// `fpassthru(): int`, `php_stream_passthru`: the rest written out.
    #[php(tentative)]
    fn fpassthru(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let (_, stream) = file_checked(req, this.object())?;
        let rest = stream.copy_to_mem(req, None)?;
        let n = rest.len();
        req.response_mut().write(rest.as_bytes());
        rest.give_up(req);
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// `fscanf(string $format, mixed &...$vars): array|int|null`: the next
    /// line through `ext/standard`'s `php_sscanf_internal`, called directly
    /// as php's method calls it, which assigns through the call's own
    /// arguments from the second on, `vars` as the frame holds them.
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
    #[php(tentative)]
    fn fscanf(
        req: &mut Request,
        this: This<Self>,
        format: PhpString,
        vars: Variadic<Ref<Value>>,
    ) -> Result<ScanAnswer, Thrown> {
        let (this, _) = file_checked(req, this.object())?;
        read_line_counted(req, this, false, false)?;
        let line = fs(req, this, |it| it.line.clone())?.unwrap_or_default();
        let scanned =
            php_ext_standard::php_sscanf_internal(req, &line, format.as_bytes(), 2, vars.len())?;
        Ok(crate::answered(scanned))
    }

    /// `fwrite(string $data, ?int $length = null): int|false`.
    #[php(tentative)]
    fn fwrite(
        req: &mut Request,
        this: This<Self>,
        data: PhpString,
        #[php(default = null)] length: Option<i64>,
    ) -> Result<OrFalse<i64>, Thrown> {
        let (_, stream) = file_checked(req, this.object())?;
        let data = data.as_bytes();
        let len = match length {
            Some(n) => usize::try_from(n).unwrap_or(0).min(data.len()),
            None => data.len(),
        };
        if len == 0 {
            return Ok(OrFalse::Value(0));
        }
        Ok(match stream.write(req, &data[..len]) {
            Some(n) => OrFalse::Value(i64::try_from(n).unwrap_or(i64::MAX)),
            None => OrFalse::False,
        })
    }

    /// `fstat(): array`, through `ext/standard`'s `php_fstat` on the stream,
    /// called directly as php's method calls it, so `disable_functions`
    /// naming `fstat` leaves it standing; the witness under that ini is
    /// `tests/capture.rs`, since a fence carries no ini of its own.
    ///
    /// ```php
    /// $t = new SplTempFileObject(); $t->fwrite("abc"); $s = $t->fstat();
    /// var_dump(count($s), $s['size'], $s[7], $s['mode'] & 0170000);
    /// ```
    /// ```expect
    /// int(26)
    /// int(3)
    /// int(3)
    /// int(32768)
    /// ```
    #[php(tentative)]
    fn fstat(req: &mut Request, this: This<Self>) -> Result<Owner<Raw<PhpArray>>, Thrown> {
        let (_, stream) = file_checked(req, this.object())?;
        let stat = php_ext_standard::php_fstat(req, stream)?;
        Ok(crate::answered(stat))
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
    #[php(tentative)]
    fn ftruncate(req: &mut Request, this: This<Self>, size: i64) -> Result<bool, Thrown> {
        let (this, stream) = file_checked(req, this.object())?;
        if size < 0 {
            return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
        }
        if let Some(done) = stream.truncate(req, usize::try_from(size).unwrap_or(0))? {
            return Ok(done);
        }
        let name =
            String::from_utf8_lossy(&fs(req, this, |it| it.file_name.clone())?.unwrap_or_default())
                .into_owned();
        Err(throw_exception_ex(
            req,
            "LogicException",
            &format!("Can't truncate file {name}"),
        ))
    }

    /// `current(): string|array|false`: the line, read where none is, or
    /// its fields under `READ_CSV`.
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<FileLine, Thrown> {
        let (this, _) = file_checked(req, this.object())?;
        if fs(req, this, |it| it.line.is_none() && it.csv.is_none())? {
            read_line(req, this, true)?;
        }
        let (line, csv, flags) = fs(req, this, |it| {
            (it.line.clone(), it.csv.as_deref().copied(), it.flags)
        })?;
        if let Some(line) = line
            && (flags & Self::READ_CSV == 0 || csv.is_none())
        {
            return Ok(OrFalse::Value(Union::A(PhpString::new(req, &line)?)));
        }
        Ok(match csv.and_then(|c| c.as_array()) {
            Some(csv) => OrFalse::Value(Union::B(Owner::take(req, csv))),
            None => OrFalse::False,
        })
    }

    /// `key(): int`, the line number.
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        fs(req, this.object(), |it| it.line_num)
    }

    /// `next(): void`.
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let (this, _) = file_checked(req, this.object())?;
        free_line(req, this)?;
        if fs(req, this, |it| it.flags & Self::READ_AHEAD != 0)? {
            read_line(req, this, true)?;
        }
        fs(req, this, |it| it.line_num += 1)
    }

    /// `setFlags(int $flags): void`.
    #[php(tentative)]
    fn setFlags(req: &mut Request, this: This<Self>, flags: i64) -> Result<(), Thrown> {
        fs(req, this.object(), |it| it.flags = flags)
    }

    /// `getFlags(): int`.
    #[php(tentative)]
    fn getFlags(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        fs(req, this.object(), |it| it.flags & 0xF)
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
    #[php(tentative)]
    fn setMaxLineLen(req: &mut Request, this: This<Self>, maxLength: i64) -> Result<(), Thrown> {
        if maxLength < 0 {
            return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
        }
        fs(req, this.object(), |it| it.max_line_len = maxLength)
    }

    /// `getMaxLineLen(): int`.
    #[php(tentative)]
    fn getMaxLineLen(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        fs(req, this.object(), |it| it.max_line_len)
    }

    /// `hasChildren(): false`.
    #[php(tentative)]
    fn hasChildren(this: This<Self>) -> False {
        let _ = this;
        False
    }

    /// `getChildren(): null`.
    #[php(tentative)]
    fn getChildren(this: This<Self>) -> Null {
        let _ = this;
        Null
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
    #[php(tentative)]
    fn seek(req: &mut Request, this: This<Self>, line: i64) -> Result<(), Thrown> {
        let (this, _) = file_checked(req, this.object())?;
        if line < 0 {
            return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
        }
        file_rewind_to_start(req, this)?;
        for _ in 0..line {
            if !read_line(req, this, true)? {
                return Ok(());
            }
        }
        if line > 0 && fs(req, this, |it| it.flags & Self::READ_AHEAD == 0)? {
            fs(req, this, |it| it.line_num += 1)?;
            free_line(req, this)?;
        }
        Ok(())
    }

    #[php(tentative, implementation = SplFileObject::fgets)]
    fn getCurrentLine(this: This<Self>) -> PhpString;

    /// `__toString(): string`, the current line.
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let (this, _) = file_checked(req, this.object())?;
        if fs(req, this, |it| it.line.is_none())? {
            read_line(req, this, false)?;
        }
        let line = fs(req, this, |it| it.line.clone())?.unwrap_or_default();
        PhpString::new(req, &line)
    }
}

/// `class SplTempFileObject extends SplFileObject`: `php://temp`,
/// `php://temp/maxmemory:N` or `php://memory`.
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
#[php_class(extends(SplFileObject))]
pub(crate) struct SplTempFileObject;

#[php_class]
impl SplTempFileObject {
    /// `SplTempFileObject::__construct(int $maxMemory = 2 * 1024 * 1024)`.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = 2 * 1024 * 1024)] maxMemory: i64,
    ) -> Result<(), Thrown> {
        let this = this.object();
        if fs(req, this, |it| it.stream.is_some())? {
            return Err(throw_error(req, "Error", "Cannot call constructor twice"));
        }
        let name: Vec<u8> = if maxMemory < 0 {
            b"php://memory".to_vec()
        } else if req.frames().argument_count() > 0 {
            format!("php://temp/maxmemory:{maxMemory}").into_bytes()
        } else {
            b"php://temp".to_vec()
        };
        file_open(req, this, &name, b"wb", false)?;
        fs(req, this, |it| it.path = Some(Vec::new()))
    }
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
    throw_error(req, "Error", "Object not initialized")
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

/// `spl_filesystem_object_get_path`: the path, or for a directory over
/// a glob stream the stream's own (`php_glob_stream_get_path`), the
/// directory of the match it last read, `NULL` where that is empty.
fn object_path(req: &mut Request, this: PhpObject) -> Result<Option<Vec<u8>>, Thrown> {
    if fs(req, this, |it| it.kind == Type::Dir)?
        && let Some(dir) = glob_stream(req, this)?
    {
        return Ok(dir.glob_path(req).filter(|path| !path.is_empty()));
    }
    fs(req, this, |it| it.path.clone())
}

/// `spl_intern_is_glob`: the directory stream, where it is a glob's.
fn glob_stream(req: &mut Request, this: PhpObject) -> Result<Option<Stream>, Thrown> {
    let dir = fs(req, this, |it| it.dir.as_ref().map(Open::stream))?;
    Ok(dir.filter(|dir| dir.glob_count(req).is_some()))
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
    let path = object_path(req, this)?;
    let name = match path {
        Some(path) if !path.is_empty() => {
            let _ = flags & FilesystemIterator::UNIX_PATHS;
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
    /// `spl_filesystem_object_free_storage`: the other handler's `dtor`,
    /// then `zend_object_std_dtor`, then what the object opened.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        if let Some(other) = object.native_mut::<Fs>(req).and_then(|it| it.oth.take()) {
            other.dtor(req);
        }
        Std.free_obj(req, object);
        let held = object
            .native_mut::<Fs>(req)
            .map(|it| (it.dir.take(), it.stream.take(), it.csv.take()));
        if let Some((dir, stream, csv)) = held {
            for open in [dir, stream].into_iter().flatten() {
                open.close(req);
            }
            if let Some(csv) = csv {
                csv.give_up(req);
            }
        }
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_fs)
    }

    /// `spl_filesystem_object_destroy_object`: the class's `__destruct`,
    /// then the directory or the file closed, so a method served on the
    /// object after its destructor finds no stream.
    ///
    /// ```php
    /// class Target { public $f; function __destruct() { try { $this->f->flock(LOCK_SH); } catch (Error $e) { echo $e->getMessage(), "\n"; } } }
    /// class Run { static $f; static $t; }
    /// Run::$f = new SplTempFileObject; Run::$t = new Target; Run::$t->f = Run::$f;
    /// ```
    /// ```expect
    /// Object not initialized
    /// ```
    ///
    /// The close gives up the object's hold on the resource, so an object
    /// unset leaves `get_resources()` as it found it (`bug70417`).
    ///
    /// ```php
    /// stream_context_get_default();
    /// $before = count(get_resources());
    /// $f = new SplFileObject('php://memory', 'w+');
    /// $t = new SplTempFileObject();
    /// $d = new DirectoryIterator(sys_get_temp_dir());
    /// unset($f, $t, $d);
    /// var_dump(count(get_resources()) - $before);
    /// ```
    /// ```expect
    /// int(0)
    /// ```
    fn dtor_obj(&self) -> DtorObj {
        DtorObj::Own(|req, object| {
            DtorObj::Destroy.serve(req, object);
            let held = object
                .native_mut::<Fs>(req)
                .map(|it| (it.dir.take(), it.stream.take()));
            for open in held
                .into_iter()
                .flat_map(|(dir, stream)| [dir, stream])
                .flatten()
            {
                open.close(req);
            }
        })
    }
}

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for FileHandlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        HANDLERS.free_obj(req, object);
    }

    fn dtor_obj(&self) -> DtorObj {
        HANDLERS.dtor_obj()
    }

    /// `spl_filesystem_object_get_method_check`: every method, the
    /// class's own and one a user class declares alike, refused with
    /// php's `Error` while the object opened neither a directory nor a
    /// file, which is an object whose constructor never ran.
    ///
    /// ```php
    /// class F extends SplFileObject { function __construct() {} function fpassthru(): int { return 0; } }
    /// class G extends GlobIterator { function __construct() {} }
    /// foreach ([new F, new G] as $o) {
    ///     try { $o->fpassthru(); } catch (Error $e) { echo count($e->getTrace()), " ", $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// 0 The parent constructor was not called: the object is in an invalid state
    /// 0 The parent constructor was not called: the object is in an invalid state
    /// ```
    fn get_method(
        &self,
        req: &mut Request,
        object: PhpObject,
        lowered: &[u8],
    ) -> Result<Option<Function>, Thrown> {
        let unopened = fs(req, object, |it| it.dir.is_none() && it.orig_path.is_none())?;
        if unopened {
            return Err(throw_error(
                req,
                "Error",
                "The parent constructor was not called: the object is in an invalid state",
            ));
        }
        Std.get_method(req, object, lowered)
    }
}

/// `spl_filesystem_object_clone`: an info copied; a directory opened
/// again and read to where it stood; a file refused, as php's
/// `ZEND_UNREACHABLE` guards.
fn clone_fs(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
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
        return Err(throw_error(
            req,
            "Error",
            "Trying to clone an uncloneable object of class SplFileObject",
        ));
    }
    let other = fs(req, object, |it| it.oth.take())?;
    let copied = other.as_ref().map(|other| other.clone_other(req));
    fs(req, object, |it| it.oth = other)?;
    let copy = Fs {
        oth: copied,
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
        let reopened = (|| {
            dir_open(req, *cloned, &path)?;
            for _ in 0..index {
                dir_step(req, *cloned)?;
            }
            fs(req, *cloned, |it| it.index = index)
        })();
        if let Err(thrown) = reopened {
            cloned.give_up(req);
            return Err(thrown);
        }
    }
    // The `clone_obj` slot's return, the one owner the copy was made with.
    Ok(cloned)
}

/// A file's name beside its stat, `php_fstat`'s thirteen fields.
type Stat = (Vec<u8>, [i64; 13]);

/// The stat of the object's file, of the link itself where `link`
/// asks, as `FileInfoFunction` makes it under `EH_THROW`: a warning
/// raised on the way, the `open_basedir` guard's among them, is the
/// `RuntimeException`; `None` where it fails.
fn stat(req: &mut Request, this: PhpObject, link: bool) -> Result<Option<Stat>, Thrown> {
    let name = file_name(req, this)?;
    let got = php_ext::raising::throwing_warnings(req, "RuntimeException", |req| {
        req.url_stat(&name, link, true)
    })?
    .found();
    Ok(got.map(|s| (name, s)))
}

/// Whether the object's file name is empty, which `php_stat` answers
/// `false` for before it stats anything and raises nothing for: a
/// `GlobIterator` standing on no entry.
fn unnamed(req: &mut Request, this: PhpObject) -> Result<bool, Thrown> {
    Ok(file_name(req, this)?.is_empty())
}

/// One field of the object's stat, php's `RuntimeException` naming the
/// method `what` where the stat fails.
fn stat_field(
    req: &mut Request,
    this: PhpObject,
    field: usize,
    what: &str,
) -> Result<OrFalse<i64>, Thrown> {
    if let Some((_, s)) = stat(req, this, false)? {
        return Ok(OrFalse::Value(s[field]));
    }
    if unnamed(req, this)? {
        return Ok(OrFalse::False);
    }
    let name = String::from_utf8_lossy(&file_name(req, this)?).into_owned();
    Err(throw(
        req,
        "RuntimeException",
        &format!("SplFileInfo::{what}(): stat failed for {name}"),
    ))
}

/// The `st_mode` test of `FS_IS_FILE`, `FS_IS_DIR` and `FS_IS_LINK`: `false`
/// for a stat that fails, which these three raise nothing for.
fn is_kind(req: &mut Request, this: PhpObject, link: bool, kind: i64) -> Result<bool, Thrown> {
    Ok(stat(req, this, link)?.is_some_and(|(_, s)| s[2] & 0o170_000 == kind))
}

/// The access test through `ext/standard`'s `php_stat`, called directly
/// as `FileInfoFunction` calls it with `FS_IS_W`, `FS_IS_R` and
/// `FS_IS_X`.
fn access(
    req: &mut Request,
    this: PhpObject,
    ty: php_ext_standard::Fs,
) -> Result<Owner<Raw<bool>>, Thrown> {
    let name = file_name(req, this)?;
    let answered = php_ext::raising::throwing_warnings(req, "RuntimeException", |req| {
        php_ext_standard::php_stat(req, &name, ty)
    })?;
    Ok(crate::answered(answered))
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

/// `Z_PARAM_CLASS` of a class derived from `base`, and
/// `Z_PARAM_CLASS_OR_NULL` where `nullable`, over the argument `given`
/// at `position`: `null` is none, and php's `TypeError` names the base
/// as the parameter's default names it.
fn class_argument(
    req: &mut Request,
    given: Value,
    position: u32,
    base: &[u8],
    nullable: bool,
) -> Result<Option<ClassId>, Thrown> {
    if matches!(given.as_ref(), ValueRef::Null | ValueRef::Undef) {
        return Ok(None);
    }
    let name = coerce_param(req, &given, &DeclType::String, position)?;
    let name = name
        .as_php_string()
        .map_or_else(Vec::new, |s| s.as_bytes().to_vec());
    let found = autoload(req, &name)?;
    match found {
        Some(class) if crate::is_a(req, class, base) => Ok(Some(class)),
        _ => {
            let shown = String::from_utf8_lossy(&name).into_owned();
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

/// A class this extension declares, by name.
fn linked(req: &mut Request, name: &[u8]) -> Result<ClassId, Thrown> {
    match lookup_class(req, name)? {
        Some(class) => Ok(class),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: an SPL file class is not linked",
        )),
    }
}

/// The info class chosen, `SplFileInfo` where none is.
fn info_class(req: &mut Request, chosen: Option<ClassId>) -> Result<ClassId, Thrown> {
    match chosen {
        Some(class) => Ok(class),
        None => linked(req, b"SplFileInfo"),
    }
}

/// `spl_filesystem_object_create_info`: an object of `class` over
/// `path`, its constructor served where it is not `SplFileInfo`'s own
/// (`ce->constructor->common.scope != spl_ce_SplFileInfo`), a user
/// class's or an extension's, as phar's `PharFileInfo` declares one.
fn create_info(req: &mut Request, path: &[u8], class: ClassId) -> Result<Owner<PhpObject>, Thrown> {
    let object = new_object(req, class)?;
    if crate::overridden(req, *object, b"__construct") || declares_constructor(req, class)? {
        let arg = crate::string_value(req, path)?;
        let served = call_method(req, &object, b"__construct", &[*arg]);
        arg.give_up(req);
        match served {
            Ok(answered) => answered.give_up(req),
            Err(thrown) => {
                object.give_up(req);
                return Err(thrown);
            }
        }
    } else {
        set_filename(req, *object, path)?;
    }
    Ok(object)
}

/// Whether a class from `class` up to `SplFileInfo`, that one aside,
/// declares its own `__construct`: `ce->constructor->common.scope !=
/// spl_ce_SplFileInfo`.
fn declares_constructor(req: &mut Request, class: ClassId) -> Result<bool, Thrown> {
    let base = linked(req, b"SplFileInfo")?;
    Ok(req
        .symbols()
        .method_row(class, b"__construct")
        .is_some_and(|row| row.scope() != base))
}

/// The name of the info class an object makes, `SplFileInfo` by default.
fn info_base(req: &mut Request, this: PhpObject) -> Result<Vec<u8>, Thrown> {
    Ok(match fs(req, this, |it| it.info_class)? {
        Some(class) => crate::class_name(req, class),
        None => b"SplFileInfo".to_vec(),
    })
}

/// `SplFileInfo::__debugInfo`'s array.
fn debug_info(req: &mut Request, this: PhpObject) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = crate::members(req, this)?;
    let path_name = pathname(req, this)?.unwrap_or_default();
    let set = |req: &mut Request,
               out: &mut Owner<PhpArray>,
               class: &[u8],
               field: &[u8],
               value: Owner<Value>| {
        let name = [b"\0".as_slice(), class, b"\0", field].concat();
        out.set_str(req, &name, value)
    };
    let value = crate::string_value(req, &path_name)?;
    set(req, &mut out, b"SplFileInfo", b"pathName", value)?;
    if fs(req, this, |it| it.file_name.is_some())? {
        let name = short_name(req, this)?;
        let value = crate::string_value(req, &name)?;
        set(req, &mut out, b"SplFileInfo", b"fileName", value)?;
    }
    let glob = glob_stream(req, this)?.is_some();
    let (kind, path, sub_path, mode, delimiter, enclosure) = fs(req, this, |it| {
        (
            it.kind,
            it.path.clone(),
            it.sub_path.clone(),
            it.open_mode.clone(),
            it.delimiter,
            it.enclosure,
        )
    })?;
    if kind == Type::Dir {
        // `intern->path` of a glob stream's iterator, the pattern as
        // `spl_filesystem_dir_open` was handed it, `glob://` first.
        let value = match path {
            Some(pattern) if glob => crate::string_value(req, &pattern)?,
            _ => Owner::bool(false),
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
    Ok(out)
}

// ---- the directory iterators

/// `spl_filesystem_dir_read`: the next entry, empty past the last.
fn dir_read(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    let dir = fs(req, this, |it| {
        it.file_name = None;
        it.dir.as_ref().map(Open::stream)
    })?;
    let entry = match dir {
        Some(dir) => dir.read_dir(req)?,
        None => None,
    };
    fs(req, this, |it| it.entry = entry.unwrap_or_default())
}

fn is_dot(entry: &[u8]) -> bool {
    entry == b"." || entry == b".."
}

/// One step: the next entry, the dots stepped over under `SKIP_DOTS`.
fn dir_step(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    let skip = fs(req, this, |it| {
        it.flags & FilesystemIterator::SKIP_DOTS != 0
    })?;
    loop {
        dir_read(req, this)?;
        let entry = fs(req, this, |it| it.entry.clone())?;
        if !(skip && is_dot(&entry)) {
            return Ok(());
        }
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
        return Err(throw_exception_ex(
            req,
            "UnexpectedValueException",
            &format!("Failed to open directory \"{shown}\""),
        ));
    };
    if let Some(old) = fs(req, this, |it| it.dir.replace(Open(dir)))? {
        old.close(req);
    }
    dir_step(req, this)
}

/// `spl_filesystem_object_construct`: an empty path refused, a second
/// call refused, and the directory opened under `flags`.
fn dir_construct(
    req: &mut Request,
    this: PhpObject,
    path: &[u8],
    flags: i64,
) -> Result<(), Thrown> {
    if path.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if fs(req, this, |it| it.path.is_some())? {
        return Err(throw_error(
            req,
            "Error",
            "Directory object is already initialized",
        ));
    }
    fs(req, this, |it| it.flags = flags)?;
    dir_open(req, this, path)
}

/// `CHECK_DIRECTORY_ITERATOR_IS_INITIALIZED`.
fn dir_checked(req: &mut Request, this: PhpObject) -> Result<PhpObject, Thrown> {
    if fs(req, this, |it| it.dir.is_none())? {
        return Err(uninitialized(req));
    }
    Ok(this)
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
        .found()
        .is_some_and(|s| s[2] & 0o170_000 == 0o040_000)
    {
        return Err(throw_exception_ex(
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
        return Err(throw_exception_ex(
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
    // `PHP_STREAM_FLAG_NO_FCLOSE`: the object keeps the stream, and
    // `orig_path` is the path the stream opened, the one the include
    // path resolved where it was searched.
    stream.forbid_fclose(req);
    let orig = stream.orig_path(req).unwrap_or_else(|| name.to_vec());
    let old = fs(req, this, |it| {
        it.file_name = Some(trimmed.to_vec());
        it.orig_path = Some(orig);
        it.open_mode = mode.to_vec();
        it.delimiter = b',';
        it.enclosure = b'"';
        it.escape = Some(b'\\');
        it.escape_set = false;
        it.stream.replace(Open(stream))
    })?;
    if let Some(old) = old {
        old.close(req);
    }
    Ok(())
}

/// `CHECK_SPL_FILE_OBJECT_IS_INITIALIZED`: the receiver and its stream.
fn file_checked(req: &mut Request, this: PhpObject) -> Result<(PhpObject, Stream), Thrown> {
    match fs(req, this, |it| it.stream.as_ref().map(Open::stream))? {
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
        csv.give_up(req);
    }
    Ok(())
}

fn cannot_read(req: &mut Request, this: PhpObject) -> Result<Thrown, Thrown> {
    let name =
        String::from_utf8_lossy(&fs(req, this, |it| it.file_name.clone())?.unwrap_or_default())
            .into_owned();
    Ok(throw_exception_ex(
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
    let (stream, max, flags) = fs(req, this, |it| {
        (
            it.stream.as_ref().map(Open::stream),
            it.max_line_len,
            it.flags,
        )
    })?;
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
    let mut line = stream.get_line(req, max)?.unwrap_or_default();
    if !csv && flags & SplFileObject::DROP_NEW_LINE != 0 && line.last() == Some(&b'\n') {
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
            || (it.flags & SplFileObject::READ_CSV != 0
                && it.flags & SplFileObject::DROP_NEW_LINE != 0
                && (line == b"\n" || line == b"\r\n"))
    })
}

/// `spl_filesystem_file_read_csv`: a line read and handed to
/// `ext/standard`'s `php_fgetcsv`, called directly with the stream
/// beside it, so a field whose enclosure is open at the line's end reads
/// on from the stream; the current line stays the one read. An empty
/// line is `php_bc_fgetcsv_empty_line`, one `null`.
fn read_csv(
    req: &mut Request,
    this: PhpObject,
    silent: bool,
    (delimiter, enclosure, escape): (u8, u8, Option<u8>),
) -> Result<bool, Thrown> {
    loop {
        if !read_line_counted(req, this, silent, true)? {
            return Ok(false);
        }
        let skip = fs(req, this, |it| it.flags & SplFileObject::SKIP_EMPTY != 0)?;
        if !(skip && line_empty(req, this)?) {
            break;
        }
    }
    let (line, stream) = fs(req, this, |it| {
        (it.line.clone(), it.stream.as_ref().map(Open::stream))
    })?;
    let line = line.unwrap_or_default();
    let mut failed = None;
    let mut more = || {
        let stream = stream?;
        match stream.get_line(req, None) {
            Ok(next) => next,
            Err(thrown) => {
                failed = Some(thrown);
                None
            }
        }
    };
    let fields = php_ext_standard::php_fgetcsv(&line, delimiter, enclosure, escape, &mut more);
    if let Some(thrown) = failed {
        return Err(thrown);
    }
    let mut parsed = PhpArray::new(req)?;
    let filled = match fields {
        None => parsed.append(req, Owner::null()),
        Some(fields) => fields.iter().try_for_each(|field| {
            let value = crate::string_value(req, field)?;
            parsed.append(req, value)
        }),
    };
    if let Err(thrown) = filled {
        parsed.give_up(req);
        return Err(thrown);
    }
    if let Some(old) = fs(req, this, |it| it.csv.replace(parsed.into_value()))? {
        old.give_up(req);
    }
    Ok(true)
}

/// `spl_filesystem_file_read_line_ex`: CSV, a user `getCurrentLine()`,
/// or a line read directly.
fn read_line_ex(req: &mut Request, this: PhpObject, silent: bool) -> Result<bool, Thrown> {
    let (flags, control) = fs(req, this, |it| {
        (it.flags, (it.delimiter, it.enclosure, it.escape))
    })?;
    if flags & SplFileObject::READ_CSV != 0 {
        return read_csv(req, this, silent, control);
    }
    if crate::overridden(req, this, b"getcurrentline") {
        free_line(req, this)?;
        let Some(stream) = fs(req, this, |it| it.stream.as_ref().map(Open::stream))? else {
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
            answered.give_up(req);
            return Err(type_error(
                req,
                &format!(
                    "{class}::getCurrentLine(): Return value must be of type string, {name} returned"
                ),
            ));
        };
        answered.give_up(req);
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
    while read
        && fs(req, this, |it| it.flags & SplFileObject::SKIP_EMPTY != 0)?
        && line_empty(req, this)?
    {
        free_line(req, this)?;
        read = read_line_ex(req, this, silent)?;
    }
    Ok(read)
}

/// `spl_filesystem_file_rewind`.
fn file_rewind_to_start(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    let Some(stream) = fs(req, this, |it| it.stream.as_ref().map(Open::stream))? else {
        return Err(uninitialized(req));
    };
    if !stream.seek(req, 0, Whence::Set) {
        let name =
            String::from_utf8_lossy(&fs(req, this, |it| it.file_name.clone())?.unwrap_or_default())
                .into_owned();
        return Err(throw_exception_ex(
            req,
            "RuntimeException",
            &format!("Cannot rewind file {name}"),
        ));
    }
    free_line(req, this)?;
    fs(req, this, |it| it.line_num = 0)?;
    if fs(req, this, |it| it.flags & SplFileObject::READ_AHEAD != 0)? {
        read_line(req, this, true)?;
    }
    Ok(())
}

/// A one-byte CSV control, the object's `default` where the call leaves
/// it out, php's `ValueError` for one of another length.
fn control_char(
    req: &mut Request,
    given: Passed<PhpString>,
    position: u32,
    default: u8,
) -> Result<u8, Thrown> {
    match given {
        Passed::Given(given) => match given.as_bytes() {
            [one] => Ok(*one),
            _ => Err(arg_value_error(req, position, "be a single character")),
        },
        Passed::Omitted => Ok(default),
    }
}

/// The escape: empty for none, one byte, and php's deprecation where the
/// call leaves it out and `setCsvControl()` never set it.
fn escape_arg(
    req: &mut Request,
    given: Passed<PhpString>,
    position: u32,
    this: PhpObject,
) -> Result<Option<u8>, Thrown> {
    if let Passed::Given(given) = given {
        return match given.as_bytes() {
            [] => Ok(None),
            [one] => Ok(Some(*one)),
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
