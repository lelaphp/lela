//! `phar_object.c`'s `Phar` and `PharData`: an archive as a
//! `RecursiveDirectoryIterator` over its `phar://` URL, the archive laid on
//! the object in the slot spl keeps for a class extending its filesystem
//! objects (`spl_filesystem_object.oth`), and the methods that read and
//! change it. `PharData`'s methods are `Phar`'s, as its stub's
//! `@implementation-alias` says, but for its constructor, which takes the
//! format.

use php_ext::abi::{OrFalse, Passed, Path as PathArg, Raw, This, True, Union, Untyped};
use php_ext::attr::php_class;
use php_ext::call::{instantiate, known_method, lookup_class};
use php_ext::coerce::{value_name, with_string};
use php_ext::decl::Name;
use php_ext::raising::{Thrown, arg_type_error, arg_value_error, arity, throw};
use php_ext::request::{OwnedStream as _, Request, Stream, Streams as _};
use php_ext::values::{ClassId, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};
use php_ext_spl::directory::{self as spl, Other};

use crate::archive::{
    self, ArchiveId, ENT_COMPRESSED_BZ2, ENT_COMPRESSED_GZ, ENT_COMPRESSION_MASK,
    FILE_COMPRESSED_BZ2, FILE_COMPRESSED_GZ, Format, Metadata, SIG_MD5, SIG_OPENSSL,
    SIG_OPENSSL_SHA256, SIG_OPENSSL_SHA512, SIG_SHA1, SIG_SHA256, SIG_SHA512, archives, lossy,
    string,
};
use crate::entry::{self, Dir};
use crate::open::{self, has_bz2, has_zlib, readonly};
use crate::path::{self, Create, Exec};
use crate::write;

/// Core's and spl's classes these extend and implement.
#[php_class(extern)]
pub(crate) struct RecursiveDirectoryIterator;
#[php_class(extern)]
pub(crate) struct SplFileInfo;
#[php_class(extern)]
pub(crate) struct Countable;
#[php_class(extern)]
pub(crate) struct ArrayAccess;
#[php_class(extern)]
pub(crate) struct Traversable;
#[php_class(extern)]
pub(crate) struct Exception;

/// `class PharException extends Exception`.
#[php_class(extends(Exception))]
pub(crate) struct PharException;

#[php_class]
impl PharException {}

/// What a `Phar` or a `PharData` holds, `phar_archive_object`'s archive:
/// one reference of it, given back as the object is freed
/// (`phar_spl_foreign_dtor`) and taken again by a clone
/// (`phar_spl_foreign_clone`).
pub(crate) struct Held(pub(crate) ArchiveId);

impl Other for Held {
    fn dtor(self: Box<Self>, req: &mut Request) {
        archive::delref(req, self.0);
    }

    fn clone_other(&self, req: &mut Request) -> Box<dyn Other> {
        if let Some(a) = archives(req).get_mut(self.0) {
            a.refcount += 1;
        }
        Box::new(Held(self.0))
    }
}

/// `PHAR_ARCHIVE_OBJECT()`: the archive the object holds, or php's
/// `BadMethodCallException` for one whose constructor never ran.
pub(crate) fn held(req: &mut Request, this: PhpObject) -> Result<ArchiveId, Thrown> {
    let id = spl::other_mut::<Held>(req, this).map(|held| held.0);
    match id.filter(|id| archives(req).get(*id).is_some()) {
        Some(id) => Ok(id),
        None => Err(throw(
            req,
            "BadMethodCallException",
            "Cannot call method on an uninitialized Phar object",
        )),
    }
}

/// The archive's name and data bit, which most refusals read.
fn named(req: &mut Request, id: ArchiveId) -> (String, bool) {
    archives(req).get(id).map_or((String::new(), false), |a| {
        (lossy(&a.fname).into_owned(), a.is_data)
    })
}

/// `PHAR_G(readonly) && !phar->is_data`: `class` thrown with `message`
/// where writes are off for this archive.
fn writes_off(req: &mut Request, id: ArchiveId, class: &str, message: &str) -> Result<(), Thrown> {
    let (_, is_data) = named(req, id);
    if readonly(req) && !is_data {
        return Err(throw(req, class, message));
    }
    Ok(())
}

/// A flush's error as `PharException`.
fn flushed(req: &mut Request, id: ArchiveId) -> Result<(), Thrown> {
    match write::flush(req, id)? {
        Some(error) => Err(throw(req, "PharException", &error)),
        None => Ok(()),
    }
}

/// A class by name, as the linker registered it.
pub(crate) fn class_named(req: &mut Request, name: &[u8]) -> Result<ClassId, Thrown> {
    match lookup_class(req, name)? {
        Some(class) => Ok(class),
        None => Err(php_ext::raising::fatal(
            req,
            &format!("InternalError: class {} is not linked", lossy(name)),
        )),
    }
}

/// `zend_call_known_instance_method_with_*(ce->constructor, obj, ...)`:
/// `class`'s own constructor served on `object`.
pub(crate) fn construct_as(
    req: &mut Request,
    class: &[u8],
    object: PhpObject,
    args: &[Value],
) -> Result<(), Thrown> {
    let class = class_named(req, class)?;
    let Some(ctor) = known_method(
        req,
        class,
        &Name(b"__construct".as_slice().into()),
        Some(object),
    ) else {
        return Ok(());
    };
    let answered = php_ext::call::call(req, &ctor, args)?;
    answered.give_up(req);
    Ok(())
}

/// `Phar::__construct` and `PharData::__construct`, one body: the archive
/// opened or created, one reference laid on the object, then
/// `RecursiveDirectoryIterator::__construct` over its `phar://` URL, and
/// `PharFileInfo` its info class.
fn construct(
    req: &mut Request,
    this: PhpObject,
    fname: &[u8],
    flags: i64,
    alias: Option<&[u8]>,
    format: i64,
) -> Result<(), Thrown> {
    let data_class = class_named(req, b"PharData")?;
    let is_data = this.instance_of(req, data_class);
    if spl::other_mut::<Held>(req, this).is_some() {
        return Err(throw(
            req,
            "BadMethodCallException",
            "Cannot call constructor twice",
        ));
    }
    let exec = if is_data {
        Exec::Data
    } else {
        Exec::Executable
    };
    let split = path::split_fname(req, fname, exec, Create::Either)?.ok();
    let (arch, inner) = match &split {
        Some(split) => (split.arch.as_slice(), Some(split.entry.clone())),
        None => (fname, None),
    };
    let id = match open::open_or_create(req, arch, alias, is_data, true)? {
        Ok(id) => id,
        Err(error) => {
            let message = error.unwrap_or_else(|| "Phar creation or opening failed".into());
            return Err(throw(req, "UnexpectedValueException", &message));
        }
    };
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(());
    };
    if is_data && a.is_tar() && a.is_brandnew && format == 3 {
        a.format = Format::Zip;
    }
    if is_data != a.is_data {
        let message = if is_data {
            "PharData class can only be used for non-executable tar and zip archives"
        } else {
            "Phar class can only be used for executable tar and zip archives"
        };
        return Err(throw(req, "UnexpectedValueException", message));
    }
    a.refcount += 1;
    let mut url = b"phar://".to_vec();
    url.extend_from_slice(&a.fname);
    if let Some(inner) = inner {
        url.extend_from_slice(&inner);
    }
    spl::set_other(req, this, Box::new(Held(id)));
    let url = string(req, &url)?;
    let constructed = construct_as(
        req,
        b"RecursiveDirectoryIterator",
        this,
        &[*url, Value::int(flags)],
    );
    url.give_up(req);
    constructed?;
    if let Some(a) = archives(req).get_mut(id) {
        a.is_data = is_data;
    }
    let info = class_named(req, b"PharFileInfo")?;
    spl::set_info_class(req, this, info);
    Ok(())
}

/// `class Phar extends RecursiveDirectoryIterator implements Countable,
/// ArrayAccess`.
///
/// ```php
/// $p = __DIR__ . '/phar-class-witness.phar';
/// $files = ['a.txt' => 'hello', 'sub/b.txt' => 'nested'];
/// $m = ''; $body = '';
/// foreach ($files as $n => $c) { $m .= pack('V', strlen($n)) . $n . pack('VVVVVV', strlen($c), 0, strlen($c), crc32($c), 0x1b6, 0); $body .= $c; }
/// $m = pack('VnVV', count($files), 0x1110, 0x10000, 0) . pack('V', 0) . $m;
/// $data = "<?php __HALT_COMPILER(); ?>\r\n" . pack('V', strlen($m)) . $m . $body;
/// file_put_contents($p, $data . sha1($data, true) . pack('V', 2) . 'GBMB');
/// $phar = new Phar($p);
/// var_dump(count($phar), $phar->getVersion(), $phar->getAlias() === $p, $phar->isFileFormat(Phar::PHAR), $phar->getSignature()['hash_type'], $phar->isCompressed(), $phar->isWritable());
/// var_dump(isset($phar['a.txt']), isset($phar['sub']), isset($phar['nope']), $phar['sub/b.txt']->getContent(), get_class($phar['a.txt']));
/// foreach (new RecursiveIteratorIterator($phar) as $k => $v) echo str_replace($p, 'P', $k), ' ', $v->getFilename(), "\n";
/// try { $phar->__construct($p); } catch (BadMethodCallException $e) { echo $e->getMessage(), "\n"; }
/// try { $phar['nope']; } catch (BadMethodCallException $e) { echo $e->getMessage(), "\n"; }
/// $x = __DIR__ . '/phar-class-witness-out';
/// var_dump($phar->extractTo($x, 'sub/b.txt', true), file_get_contents("$x/sub/b.txt"));
/// unlink("$x/sub/b.txt"); rmdir("$x/sub"); rmdir($x);
/// unset($phar); unlink($p);
/// ```
/// ```expect
/// int(2)
/// string(5) "1.1.1"
/// bool(true)
/// bool(true)
/// string(5) "SHA-1"
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// string(6) "nested"
/// string(12) "PharFileInfo"
/// phar://P/a.txt a.txt
/// phar://P/sub/b.txt b.txt
/// Cannot call constructor twice
/// Entry nope does not exist
/// bool(true)
/// string(6) "nested"
/// ```
#[php_class(
    extends(RecursiveDirectoryIterator),
    implements(Countable, ArrayAccess)
)]
pub(crate) struct Phar;

/// `Phar`'s methods, in the stub's order. Every write is refused while
/// `phar.readonly` is on, the recipe's:
///
/// ```php
/// $p = __DIR__ . '/phar-readonly-witness.phar';
/// $m = pack('V', 5) . 'a.txt' . pack('VVVVVV', 5, 0, 5, crc32('hello'), 0x1b6, 0);
/// $m = pack('VnVV', 1, 0x1110, 0x10000, 0) . pack('V', 0) . $m;
/// $data = "<?php __HALT_COMPILER(); ?>\r\n" . pack('V', strlen($m)) . $m . 'hello';
/// file_put_contents($p, $data . sha1($data, true) . pack('V', 2) . 'GBMB');
/// $phar = new Phar($p);
/// $calls = [
///     'addEmptyDir' => fn() => $phar->addEmptyDir('d'),
///     'addFile' => fn() => $phar->addFile(__FILE__, 'f'),
///     'addFromString' => fn() => $phar->addFromString('f', 'x'),
///     'buildFromDirectory' => fn() => $phar->buildFromDirectory(__DIR__),
///     'buildFromIterator' => fn() => $phar->buildFromIterator(new ArrayIterator([])),
///     'compressFiles' => fn() => $phar->compressFiles(Phar::GZ),
///     'decompressFiles' => fn() => $phar->decompressFiles(),
///     'compress' => fn() => $phar->compress(Phar::GZ),
///     'decompress' => fn() => $phar->decompress(),
///     'convertToExecutable' => fn() => $phar->convertToExecutable(),
///     'convertToData' => fn() => $phar->convertToData(),
///     'copy' => fn() => $phar->copy('a.txt', 'b.txt'),
///     'delete' => fn() => $phar->delete('a.txt'),
///     'delMetadata' => fn() => $phar->delMetadata(),
///     'setAlias' => fn() => $phar->setAlias('x'),
///     'setDefaultStub' => fn() => $phar->setDefaultStub(),
///     'setMetadata' => fn() => $phar->setMetadata(1),
///     'setSignatureAlgorithm' => fn() => $phar->setSignatureAlgorithm(Phar::MD5),
///     'setStub' => fn() => $phar->setStub('<?php __HALT_COMPILER();'),
///     'startBuffering' => fn() => $phar->startBuffering(),
///     'stopBuffering' => fn() => $phar->stopBuffering(),
///     'offsetSet' => fn() => $phar['x'] = 'y',
///     'offsetUnset' => function () use ($phar) { unset($phar['a.txt']); },
/// ];
/// foreach ($calls as $name => $f) {
///     try { $f(); echo "$name: ok\n"; } catch (Exception $e) { echo "$name: ", get_class($e), ': ', str_replace($p, 'P', $e->getMessage()), "\n"; }
/// }
/// var_dump($phar->isBuffering(), $phar->getModified(), $phar->hasMetadata(), $phar->getMetadata(), $phar->getPath() === $p, $phar->getStub());
/// unset($phar); unlink($p);
/// ```
/// ```expect
/// addEmptyDir: BadMethodCallException: Directory d does not exist and cannot be created: phar error: file "d" in phar "P" cannot be opened for writing, disabled by ini setting
/// addFile: BadMethodCallException: Entry f does not exist and cannot be created: phar error: file "f" in phar "P" cannot be opened for writing, disabled by ini setting
/// addFromString: BadMethodCallException: Entry f does not exist and cannot be created: phar error: file "f" in phar "P" cannot be opened for writing, disabled by ini setting
/// buildFromDirectory: UnexpectedValueException: Cannot write to archive - write operations restricted by INI setting
/// buildFromIterator: UnexpectedValueException: Cannot write out phar archive, phar is read-only
/// compressFiles: BadMethodCallException: Phar is readonly, cannot change compression
/// decompressFiles: BadMethodCallException: Phar is readonly, cannot change compression
/// compress: UnexpectedValueException: Cannot compress phar archive, phar is read-only
/// decompress: UnexpectedValueException: Cannot decompress phar archive, phar is read-only
/// convertToExecutable: UnexpectedValueException: Cannot write out executable phar archive, phar is read-only
/// convertToData: UnexpectedValueException: Cannot write out data phar archive, use Phar::TAR or Phar::ZIP
/// copy: UnexpectedValueException: Cannot copy "a.txt" to "b.txt", phar is read-only
/// delete: UnexpectedValueException: Cannot write out phar archive, phar is read-only
/// delMetadata: BadMethodCallException: Write operations disabled by the php.ini setting phar.readonly
/// setAlias: UnexpectedValueException: Cannot write out phar archive, phar is read-only
/// setDefaultStub: UnexpectedValueException: Cannot change stub: phar.readonly=1
/// setMetadata: BadMethodCallException: Write operations disabled by the php.ini setting phar.readonly
/// setSignatureAlgorithm: UnexpectedValueException: Cannot set signature algorithm, phar is read-only
/// setStub: UnexpectedValueException: Cannot change stub, phar is read-only
/// startBuffering: ok
/// stopBuffering: UnexpectedValueException: Cannot write out phar archive, phar is read-only
/// offsetSet: BadMethodCallException: Write operations disabled by the php.ini setting phar.readonly
/// offsetUnset: BadMethodCallException: Write operations disabled by the php.ini setting phar.readonly
/// bool(true)
/// bool(false)
/// bool(false)
/// NULL
/// bool(true)
/// string(29) "<?php __HALT_COMPILER(); ?>
/// "
/// ```
///
/// The static methods:
///
/// ```php
/// var_dump(Phar::apiVersion(), Phar::canWrite(), Phar::canCompress(Phar::GZ), in_array('GZ', Phar::getSupportedCompression()), in_array('SHA-512', Phar::getSupportedSignatures()));
/// var_dump(strlen(Phar::createDefaultStub()), strlen(Phar::createDefaultStub('i.php', 'w.php')), Phar::isValidPharFilename('a.phar'), Phar::isValidPharFilename('a.tar.gz', false), Phar::isValidPharFilename('a.txt'), Phar::running());
/// try { Phar::createDefaultStub(str_repeat('x', 401)); } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// $calls = [
///     'loadPhar' => fn() => Phar::loadPhar(__DIR__ . '/none.phar'),
///     'mapPhar' => fn() => Phar::mapPhar(),
///     'mount' => fn() => Phar::mount('in', __DIR__ . '/none'),
///     'mungServer' => fn() => Phar::mungServer([]),
///     'unlinkArchive' => fn() => Phar::unlinkArchive(__DIR__ . '/none.phar'),
///     'webPhar' => fn() => Phar::webPhar(),
///     'interceptFileFuncs' => fn() => Phar::interceptFileFuncs(),
/// ];
/// foreach ($calls as $name => $f) {
///     try { var_dump($f()); } catch (Exception $e) { echo "$name: ", get_class($e), ': ', str_replace(__DIR__, 'D', $e->getMessage()), "\n"; }
/// }
/// ```
/// ```expect
/// string(5) "1.1.1"
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// int(6641)
/// int(6633)
/// bool(true)
/// bool(true)
/// bool(false)
/// string(0) ""
/// PharException: Illegal filename passed in for stub creation, was 401 characters long, and only 400 or less is allowed
/// loadPhar: PharException: unable to open phar for reading "D/none.phar"
/// mapPhar: PharException: __HALT_COMPILER(); must be declared in a phar
/// mount: PharException: Mounting of in to D/none failed
/// mungServer: PharException: No values passed to Phar::mungServer(), expecting an array of any of these strings: PHP_SELF, REQUEST_URI, SCRIPT_FILENAME, SCRIPT_NAME
/// unlinkArchive: PharException: Unknown phar archive "D/none.phar": unable to open phar for reading "D/none.phar"
/// webPhar: PharException: __HALT_COMPILER(); must be declared in a phar
/// NULL
/// ```
#[php_class]
impl Phar {
    const BZ2: i64 = 0x2000;
    const GZ: i64 = 0x1000;
    const NONE: i64 = 0;
    const PHAR: i64 = 1;
    const TAR: i64 = 2;
    const ZIP: i64 = 3;
    const COMPRESSED: i64 = 0xF000;
    const PHP: i64 = 0;
    const PHPS: i64 = 1;
    const MD5: i64 = 1;
    const OPENSSL: i64 = 16;
    const OPENSSL_SHA256: i64 = 17;
    const OPENSSL_SHA512: i64 = 18;
    const SHA1: i64 = 2;
    const SHA256: i64 = 3;
    const SHA512: i64 = 4;

    /// `Phar::__construct(string $filename, int $flags = SKIP_DOTS |
    /// UNIX_PATHS, ?string $alias = null)`.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        filename: PathArg,
        #[php(default = FilesystemIterator::SKIP_DOTS|FilesystemIterator::UNIX_PATHS)] flags: i64,
        #[php(default = null)] alias: Option<PhpString>,
    ) -> Result<(), Thrown> {
        let alias = alias.map(|a| a.as_bytes().to_vec());
        construct(
            req,
            this.object(),
            filename.0.as_bytes(),
            flags,
            alias.as_deref(),
            0,
        )
    }

    /// `Phar::__destruct()`: a cached archive's object forgotten, which
    /// no archive here is.
    fn __destruct(this: This<Self>) {
        let _ = this;
    }

    /// `addEmptyDir(string $directory): void`.
    #[php(tentative)]
    fn addEmptyDir(req: &mut Request, this: This<Self>, directory: PathArg) -> Result<(), Thrown> {
        let id = held(req, this.object())?;
        let dir = directory.0.as_bytes().to_vec();
        if path::is_magic(&dir) {
            return Err(throw(
                req,
                "BadMethodCallException",
                "Cannot create a directory in magic \".phar\" directory",
            ));
        }
        crate::build::mkdir(req, id, &dir)
    }

    /// `addFile(string $filename, ?string $localName = null): void`.
    #[php(tentative)]
    fn addFile(
        req: &mut Request,
        this: This<Self>,
        filename: PathArg,
        #[php(default = null)] localName: Option<PhpString>,
    ) -> Result<(), Thrown> {
        let id = held(req, this.object())?;
        let file = filename.0.as_bytes().to_vec();
        if !file.windows(3).any(|w| w == b"://") && req.check_open_basedir(&file, true).is_err() {
            return Err(throw(
                req,
                "RuntimeException",
                &format!(
                    "phar error: unable to open file \"{}\" to add to phar archive, open_basedir restrictions prevent this",
                    lossy(&file)
                ),
            ));
        }
        let opening = php_ext::request::Opening::default();
        let Some(stream) = req.open_stream_with(&file, b"rb", opening)? else {
            return Err(throw(
                req,
                "RuntimeException",
                &format!(
                    "phar error: unable to open file \"{}\" to add to phar archive",
                    lossy(&file)
                ),
            ));
        };
        let name = localName.map_or(file, |n| n.as_bytes().to_vec());
        let added = crate::build::add_file(req, id, &name, crate::build::Source::Stream(*stream));
        stream.free(req);
        added
    }

    /// `addFromString(string $localName, string $contents): void`.
    #[php(tentative)]
    fn addFromString(
        req: &mut Request,
        this: This<Self>,
        localName: PathArg,
        contents: PhpString,
    ) -> Result<(), Thrown> {
        let id = held(req, this.object())?;
        let name = localName.0.as_bytes().to_vec();
        let contents = contents.as_bytes().to_vec();
        crate::build::add_file(req, id, &name, crate::build::Source::Bytes(&contents))
    }

    /// `buildFromDirectory(string $directory, string $pattern = ""): array`.
    #[php(tentative)]
    fn buildFromDirectory(
        req: &mut Request,
        this: This<Self>,
        directory: PathArg,
        #[php(default = "")] pattern: PhpString,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let id = held(req, this.object())?;
        writes_off(
            req,
            id,
            "UnexpectedValueException",
            "Cannot write to archive - write operations restricted by INI setting",
        )?;
        crate::build::from_directory(req, id, directory.0, pattern)
    }

    /// `buildFromIterator(Traversable $iterator, ?string $baseDirectory =
    /// null): array`.
    #[php(tentative)]
    fn buildFromIterator(
        req: &mut Request,
        this: This<Self>,
        iterator: php_ext::abi::Instance<Traversable>,
        #[php(default = null)] baseDirectory: Option<PhpString>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let id = held(req, this.object())?;
        writes_off(
            req,
            id,
            "UnexpectedValueException",
            "Cannot write out phar archive, phar is read-only",
        )?;
        let base = baseDirectory
            .map(|b| b.as_bytes().to_vec())
            .unwrap_or_default();
        crate::build::from_iterator(req, id, iterator.object(), &base)
    }

    /// `compressFiles(int $compression): void`: every entry's compression
    /// flags set, the archive written.
    #[php(tentative)]
    fn compressFiles(req: &mut Request, this: This<Self>, compression: i64) -> Result<(), Thrown> {
        let id = held(req, this.object())?;
        writes_off(
            req,
            id,
            "BadMethodCallException",
            "Phar is readonly, cannot change compression",
        )?;
        let flags = match compression {
            0x1000 if !has_zlib(req) => {
                return Err(throw(
                    req,
                    "BadMethodCallException",
                    "Cannot compress files within archive with gzip, enable ext/zlib in php.ini",
                ));
            }
            0x2000 if !has_bz2(req) => {
                return Err(throw(
                    req,
                    "BadMethodCallException",
                    "Cannot compress files within archive with bz2, enable ext/bz2 in php.ini",
                ));
            }
            0x1000 => ENT_COMPRESSED_GZ,
            0x2000 => ENT_COMPRESSED_BZ2,
            _ => {
                return Err(throw(
                    req,
                    "BadMethodCallException",
                    "Unknown compression specified, please pass one of Phar::GZ or Phar::BZ2",
                ));
            }
        };
        if archives(req).get(id).is_some_and(archive::Archive::is_tar) {
            return Err(throw(
                req,
                "BadMethodCallException",
                "Cannot compress with Gzip compression, tar archives cannot compress individual files, use compress() to compress the whole archive",
            ));
        }
        if !can_decompress_all(req, id) {
            let message = if flags == ENT_COMPRESSED_GZ {
                "Cannot compress all files as Gzip, some are compressed as bzip2 and cannot be decompressed"
            } else {
                "Cannot compress all files as Bzip2, some are compressed as gzip and cannot be decompressed"
            };
            return Err(throw(req, "BadMethodCallException", message));
        }
        set_compression(req, id, flags);
        match write::flush(req, id)? {
            Some(error) => Err(throw(req, "BadMethodCallException", &error)),
            None => Ok(()),
        }
    }

    /// `decompressFiles(): true`.
    #[php(tentative)]
    fn decompressFiles(req: &mut Request, this: This<Self>) -> Result<True, Thrown> {
        let id = held(req, this.object())?;
        writes_off(
            req,
            id,
            "BadMethodCallException",
            "Phar is readonly, cannot change compression",
        )?;
        if !can_decompress_all(req, id) {
            return Err(throw(
                req,
                "BadMethodCallException",
                "Cannot decompress all files, some are compressed as bzip2 or gzip and cannot be decompressed",
            ));
        }
        if archives(req).get(id).is_some_and(archive::Archive::is_tar) {
            return Ok(True);
        }
        set_compression(req, id, 0);
        match write::flush(req, id)? {
            Some(error) => Err(throw(req, "BadMethodCallException", &error)),
            None => Ok(True),
        }
    }

    /// `compress(int $compression, ?string $extension = null): ?Phar`.
    #[php(tentative)]
    fn compress(
        req: &mut Request,
        this: This<Self>,
        compression: i64,
        #[php(default = null)] extension: Option<PhpString>,
    ) -> Result<Option<Owner<php_ext::abi::Instance<Phar>>>, Thrown> {
        let id = held(req, this.object())?;
        writes_off(
            req,
            id,
            "UnexpectedValueException",
            "Cannot compress phar archive, phar is read-only",
        )?;
        if archives(req).get(id).is_some_and(archive::Archive::is_zip) {
            return Err(throw(
                req,
                "UnexpectedValueException",
                "Cannot compress zip-based archives with whole-archive compression",
            ));
        }
        let flags = match compression {
            0 => 0,
            0x1000 if !has_zlib(req) => {
                return Err(throw(
                    req,
                    "BadMethodCallException",
                    "Cannot compress entire archive with gzip, enable ext/zlib in php.ini",
                ));
            }
            0x1000 => FILE_COMPRESSED_GZ,
            0x2000 if !has_bz2(req) => {
                return Err(throw(
                    req,
                    "BadMethodCallException",
                    "Cannot compress entire archive with bz2, enable ext/bz2 in php.ini",
                ));
            }
            0x2000 => FILE_COMPRESSED_BZ2,
            _ => {
                return Err(throw(
                    req,
                    "BadMethodCallException",
                    "Unknown compression specified, please pass one of Phar::GZ or Phar::BZ2",
                ));
            }
        };
        let format = if archives(req).get(id).is_some_and(archive::Archive::is_tar) {
            Format::Tar
        } else {
            Format::Phar
        };
        let ext = extension.map(|e| e.as_bytes().to_vec());
        Ok(
            crate::convert::to_other(req, id, format, ext.as_deref(), flags)?
                .map(Owner::into_instance),
        )
    }

    /// `decompress(?string $extension = null): ?Phar`.
    #[php(tentative)]
    fn decompress(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] extension: Option<PhpString>,
    ) -> Result<Option<Owner<php_ext::abi::Instance<Phar>>>, Thrown> {
        let id = held(req, this.object())?;
        writes_off(
            req,
            id,
            "UnexpectedValueException",
            "Cannot decompress phar archive, phar is read-only",
        )?;
        if archives(req).get(id).is_some_and(archive::Archive::is_zip) {
            return Err(throw(
                req,
                "UnexpectedValueException",
                "Cannot decompress zip-based archives with whole-archive compression",
            ));
        }
        let format = if archives(req).get(id).is_some_and(archive::Archive::is_tar) {
            Format::Tar
        } else {
            Format::Phar
        };
        let ext = extension.map(|e| e.as_bytes().to_vec());
        Ok(crate::convert::to_other(req, id, format, ext.as_deref(), 0)?.map(Owner::into_instance))
    }

    /// `convertToExecutable(?int $format = null, ?int $compression = null,
    /// ?string $extension = null): ?Phar`.
    #[php(tentative)]
    fn convertToExecutable(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] format: Option<i64>,
        #[php(default = null)] compression: Option<i64>,
        #[php(default = null)] extension: Option<PhpString>,
    ) -> Result<Option<Owner<php_ext::abi::Instance<Phar>>>, Thrown> {
        let id = held(req, this.object())?;
        if readonly(req) {
            return Err(throw(
                req,
                "UnexpectedValueException",
                "Cannot write out executable phar archive, phar is read-only",
            ));
        }
        let ext = extension.map(|e| e.as_bytes().to_vec());
        let converted =
            crate::convert::convert(req, id, false, format, compression, ext.as_deref())?;
        Ok(converted.map(Owner::into_instance))
    }

    /// `convertToData(?int $format = null, ?int $compression = null,
    /// ?string $extension = null): ?PharData`.
    #[php(tentative)]
    fn convertToData(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] format: Option<i64>,
        #[php(default = null)] compression: Option<i64>,
        #[php(default = null)] extension: Option<PhpString>,
    ) -> Result<Option<Owner<php_ext::abi::Instance<PharData>>>, Thrown> {
        let id = held(req, this.object())?;
        let ext = extension.map(|e| e.as_bytes().to_vec());
        let converted =
            crate::convert::convert(req, id, true, format, compression, ext.as_deref())?;
        Ok(converted.map(Owner::into_instance))
    }

    /// `copy(string $from, string $to): true`.
    #[php(tentative)]
    fn copy(
        req: &mut Request,
        this: This<Self>,
        from: PathArg,
        to: PathArg,
    ) -> Result<True, Thrown> {
        let id = held(req, this.object())?;
        let (old, new) = (from.0.as_bytes().to_vec(), to.0.as_bytes().to_vec());
        crate::build::copy(req, id, &old, &new)?;
        Ok(True)
    }

    /// `count(int $mode = COUNT_NORMAL): int`: the manifest's rows.
    #[php(tentative)]
    fn count(
        req: &mut Request,
        this: This<Self>,
        #[php(default = COUNT_NORMAL)] mode: i64,
    ) -> Result<i64, Thrown> {
        let _ = mode;
        let id = held(req, this.object())?;
        Ok(archives(req)
            .get(id)
            .map_or(0, |a| i64::try_from(a.manifest.len()).unwrap_or(i64::MAX)))
    }

    /// `delete(string $localName): true`.
    #[php(tentative)]
    fn delete(req: &mut Request, this: This<Self>, localName: PathArg) -> Result<True, Thrown> {
        let id = held(req, this.object())?;
        writes_off(
            req,
            id,
            "UnexpectedValueException",
            "Cannot write out phar archive, phar is read-only",
        )?;
        let name = localName.0.as_bytes().to_vec();
        let Some(a) = archives(req).get_mut(id) else {
            return Ok(True);
        };
        match a.manifest.get_mut(&name) {
            Some(e) if e.is_deleted => return Ok(True),
            Some(e) => {
                e.is_deleted = true;
                e.is_modified = true;
                a.is_modified = true;
            }
            None => {
                return Err(throw(
                    req,
                    "BadMethodCallException",
                    &format!(
                        "Entry {} does not exist and cannot be deleted",
                        lossy(&name)
                    ),
                ));
            }
        }
        flushed(req, id)?;
        Ok(True)
    }

    /// `delMetadata(): true`.
    #[php(tentative)]
    fn delMetadata(req: &mut Request, this: This<Self>) -> Result<True, Thrown> {
        let id = held(req, this.object())?;
        writes_off(
            req,
            id,
            "BadMethodCallException",
            "Write operations disabled by the php.ini setting phar.readonly",
        )?;
        let Some(a) = archives(req).get_mut(id) else {
            return Ok(True);
        };
        if !a.metadata.has_data() {
            return Ok(True);
        }
        let mut old = std::mem::take(&mut a.metadata);
        a.is_modified = true;
        old.free(req);
        flushed(req, id)?;
        Ok(True)
    }

    /// `extractTo(string $directory, array|string|null $files = null, bool
    /// $overwrite = false): bool`.
    #[php(tentative)]
    fn extractTo(
        req: &mut Request,
        this: This<Self>,
        directory: PathArg,
        #[php(default = null)] files: Option<Union<PhpArray, PhpString>>,
        #[php(default = false)] overwrite: bool,
    ) -> Result<bool, Thrown> {
        let id = held(req, this.object())?;
        crate::extract::extract_to(req, id, directory.0.as_bytes(), files, overwrite)
    }

    /// `getAlias(): ?string`.
    #[php(tentative)]
    fn getAlias(req: &mut Request, this: This<Self>) -> Result<Option<Owner<PhpString>>, Thrown> {
        let id = held(req, this.object())?;
        let alias = archives(req).get(id).and_then(|a| a.alias.clone());
        alias.map(|a| PhpString::new(req, &a)).transpose()
    }

    /// `getPath(): string`: the archive's file.
    #[php(tentative)]
    fn getPath(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let id = held(req, this.object())?;
        let fname = archives(req)
            .get(id)
            .map(|a| a.fname.clone())
            .unwrap_or_default();
        PhpString::new(req, &fname)
    }

    /// `getMetadata(array $unserializeOptions = []): mixed`.
    #[php(tentative)]
    fn getMetadata(
        req: &mut Request,
        this: This<Self>,
        #[php(default = [])] unserializeOptions: Passed<PhpArray>,
    ) -> Result<Owner<Value>, Thrown> {
        let id = held(req, this.object())?;
        let Some(a) = archives(req).get_mut(id) else {
            return Ok(Owner::null());
        };
        let mut meta = std::mem::take(&mut a.metadata);
        let value = metadata_value(req, &mut meta, unserializeOptions, "Phar::getMetadata");
        match archives(req).get_mut(id) {
            Some(a) => a.metadata = meta,
            None => meta.free(req),
        }
        value
    }

    /// `getModified(): bool`.
    #[php(tentative)]
    fn getModified(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let id = held(req, this.object())?;
        Ok(archives(req).get(id).is_some_and(|a| a.is_modified))
    }

    /// `getSignature(): array|false`: the hash as upper-case hex and its
    /// type's name.
    #[php(tentative)]
    fn getSignature(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
        let id = held(req, this.object())?;
        let Some(a) = archives(req).get(id) else {
            return Ok(OrFalse::False);
        };
        let Some(hash) = a.signature.clone() else {
            return Ok(OrFalse::False);
        };
        let kind = match a.sig_flags {
            SIG_MD5 => "MD5".to_string(),
            SIG_SHA1 => "SHA-1".to_string(),
            SIG_SHA256 => "SHA-256".to_string(),
            SIG_SHA512 => "SHA-512".to_string(),
            SIG_OPENSSL => "OpenSSL".to_string(),
            SIG_OPENSSL_SHA256 => "OpenSSL_SHA256".to_string(),
            SIG_OPENSSL_SHA512 => "OpenSSL_SHA512".to_string(),
            other => format!("Unknown ({other})"),
        };
        let mut out = PhpArray::new(req)?;
        for (key, value) in [(&b"hash"[..], hash), (b"hash_type", kind.into_bytes())] {
            let value = string(req, &value)?;
            out.set_str(req, key, value)?;
        }
        Ok(OrFalse::Value(out))
    }

    /// `getStub(): string`.
    #[php(tentative)]
    fn getStub(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let id = held(req, this.object())?;
        let stub = crate::build::stub_of(req, id)?;
        PhpString::new(req, &stub)
    }

    /// `getVersion(): string`.
    #[php(tentative)]
    fn getVersion(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let id = held(req, this.object())?;
        let version = archives(req)
            .get(id)
            .map(|a| a.version.clone())
            .unwrap_or_default();
        PhpString::new(req, version.as_bytes())
    }

    /// `hasMetadata(): bool`.
    #[php(tentative)]
    fn hasMetadata(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let id = held(req, this.object())?;
        Ok(archives(req).get(id).is_some_and(|a| a.metadata.has_data()))
    }

    /// `isBuffering(): bool`.
    #[php(tentative)]
    fn isBuffering(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let id = held(req, this.object())?;
        Ok(archives(req).get(id).is_some_and(|a| a.donotflush))
    }

    /// `isCompressed(): int|false`.
    #[php(tentative)]
    fn isCompressed(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        let id = held(req, this.object())?;
        let flags = archives(req).get(id).map_or(0, |a| a.flags);
        Ok(if flags & FILE_COMPRESSED_GZ != 0 {
            OrFalse::Value(0x1000)
        } else if flags & FILE_COMPRESSED_BZ2 != 0 {
            OrFalse::Value(0x2000)
        } else {
            OrFalse::False
        })
    }

    /// `isFileFormat(int $format): bool`.
    #[php(tentative)]
    fn isFileFormat(req: &mut Request, this: This<Self>, format: i64) -> Result<bool, Thrown> {
        let id = held(req, this.object())?;
        let current = archives(req).get(id).map_or(Format::Phar, |a| a.format);
        match format {
            1 => Ok(current == Format::Phar),
            2 => Ok(current == Format::Tar),
            3 => Ok(current == Format::Zip),
            _ => Err(throw(req, "PharException", "Unknown file format specified")),
        }
    }

    /// `isWritable(): bool`: writable, and the file is where a write bit
    /// is set, or not there yet for a new archive.
    #[php(tentative)]
    fn isWritable(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let id = held(req, this.object())?;
        let Some(a) = archives(req).get(id) else {
            return Ok(false);
        };
        if !a.is_writeable {
            return Ok(false);
        }
        let (fname, brandnew) = (a.fname.clone(), a.is_brandnew);
        Ok(match req.url_stat(&fname, false, false)?.found() {
            Some(sb) => sb[2] & 0o222 != 0,
            None => brandnew,
        })
    }

    /// `offsetExists($localName): bool`.
    #[php(tentative)]
    fn offsetExists(
        req: &mut Request,
        this: This<Self>,
        localName: Untyped<PathArg>,
    ) -> Result<bool, Thrown> {
        let id = held(req, this.object())?;
        let name = localName.0.0.as_bytes().to_vec();
        let info = spl::info_class_of(req, this.object());
        let entry_class = class_named(req, b"PharFileInfo")?;
        let Some(a) = archives(req).get(id) else {
            return Ok(false);
        };
        if let Some(e) = a.manifest.get(&name) {
            return Ok(!e.is_deleted && !path::is_magic(&name));
        }
        if !info.is_some_and(|class| extends(req, class, entry_class)) {
            return Ok(false);
        }
        Ok(archives(req)
            .get(id)
            .is_some_and(|a| a.virtual_dirs.contains(&name)))
    }

    /// `offsetGet($localName): SplFileInfo`: an object of the info class
    /// over the entry's URL.
    #[php(tentative)]
    fn offsetGet(
        req: &mut Request,
        this: This<Self>,
        localName: Untyped<PathArg>,
    ) -> Result<Owner<php_ext::abi::Instance<SplFileInfo>>, Thrown> {
        let id = held(req, this.object())?;
        let name = localName.0.0.as_bytes().to_vec();
        let fname = named(req, id).0;
        match entry::find(req, id, &name, Dir::Allow, false)? {
            Ok(_) => {}
            Err(error) => {
                let tail = error.map(|e| format!(", {e}")).unwrap_or_default();
                return Err(throw(
                    req,
                    "BadMethodCallException",
                    &format!("Entry {} does not exist{tail}", lossy(&name)),
                ));
            }
        }
        if name == b".phar/stub.php" {
            return Err(throw(
                req,
                "BadMethodCallException",
                &format!(
                    "Cannot get stub \".phar/stub.php\" directly in phar \"{fname}\", use getStub"
                ),
            ));
        }
        if name == b".phar/alias.txt" {
            return Err(throw(
                req,
                "BadMethodCallException",
                &format!(
                    "Cannot get alias \".phar/alias.txt\" directly in phar \"{fname}\", use getAlias"
                ),
            ));
        }
        if path::is_magic(&name) {
            return Err(throw(
                req,
                "BadMethodCallException",
                "Cannot directly get any files or directories in magic \".phar\" directory",
            ));
        }
        let url = [b"phar://".as_slice(), fname.as_bytes(), b"/", &name].concat();
        let class = match spl::info_class_of(req, this.object()) {
            Some(class) => class,
            None => class_named(req, b"SplFileInfo")?,
        };
        let url = string(req, &url)?;
        let made = instantiate(req, class, &[*url]);
        url.give_up(req);
        Ok(made?.into_instance())
    }

    /// `offsetSet($localName, $value): void`: a string or a stream's
    /// contents added as the entry.
    #[php(tentative, arity = manual)]
    fn offsetSet(
        req: &mut Request,
        this: This<Self>,
        localName: Raw<Untyped<Value>>,
        value: Raw<Untyped<Value>>,
    ) -> Result<(), Thrown> {
        let (name, source) = zpp_name_and_value(req, localName.value(), value.value())?;
        let id = held(req, this.object())?;
        writes_off(
            req,
            id,
            "BadMethodCallException",
            "Write operations disabled by the php.ini setting phar.readonly",
        )?;
        let fname = named(req, id).0;
        if name == b".phar/stub.php" {
            return Err(throw(
                req,
                "BadMethodCallException",
                &format!(
                    "Cannot set stub \".phar/stub.php\" directly in phar \"{fname}\", use setStub"
                ),
            ));
        }
        if name == b".phar/alias.txt" {
            return Err(throw(
                req,
                "BadMethodCallException",
                &format!(
                    "Cannot set alias \".phar/alias.txt\" directly in phar \"{fname}\", use setAlias"
                ),
            ));
        }
        if path::is_magic(&name) {
            return Err(throw(
                req,
                "BadMethodCallException",
                "Cannot set any files or directories in magic \".phar\" directory",
            ));
        }
        match source {
            NameValue::Stream(stream) => {
                crate::build::add_file(req, id, &name, crate::build::Source::Stream(stream))
            }
            NameValue::Bytes(bytes) => {
                crate::build::add_file(req, id, &name, crate::build::Source::Bytes(&bytes))
            }
        }
    }

    /// `offsetUnset($localName): void`.
    #[php(tentative)]
    fn offsetUnset(
        req: &mut Request,
        this: This<Self>,
        localName: Untyped<PathArg>,
    ) -> Result<(), Thrown> {
        let id = held(req, this.object())?;
        writes_off(
            req,
            id,
            "BadMethodCallException",
            "Write operations disabled by the php.ini setting phar.readonly",
        )?;
        let name = localName.0.0.as_bytes().to_vec();
        let Some(e) = archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.get_mut(&name))
        else {
            return Ok(());
        };
        if e.is_deleted {
            return Ok(());
        }
        e.is_modified = false;
        e.is_deleted = true;
        flushed(req, id)
    }

    /// `setAlias(string $alias): true`.
    #[php(tentative)]
    fn setAlias(req: &mut Request, this: This<Self>, alias: PhpString) -> Result<True, Thrown> {
        let id = held(req, this.object())?;
        crate::build::set_alias(req, id, alias.as_bytes())?;
        Ok(True)
    }

    /// `setDefaultStub(?string $index = null, ?string $webIndex = null):
    /// true`.
    #[php(tentative)]
    fn setDefaultStub(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] index: Option<PhpString>,
        #[php(default = null)] webIndex: Option<PhpString>,
    ) -> Result<True, Thrown> {
        let id = held(req, this.object())?;
        let index = index.map(|s| s.as_bytes().to_vec());
        let web = webIndex.map(|s| s.as_bytes().to_vec());
        crate::build::set_default_stub(req, id, index.as_deref(), web.as_deref())?;
        Ok(True)
    }

    /// `setMetadata(mixed $metadata): void`.
    #[php(tentative)]
    fn setMetadata(req: &mut Request, this: This<Self>, metadata: Value) -> Result<(), Thrown> {
        let id = held(req, this.object())?;
        writes_off(
            req,
            id,
            "BadMethodCallException",
            "Write operations disabled by the php.ini setting phar.readonly",
        )?;
        let Some(a) = archives(req).get_mut(id) else {
            return Ok(());
        };
        let mut meta = std::mem::take(&mut a.metadata);
        let set = set_metadata(req, &mut meta, &metadata);
        match archives(req).get_mut(id) {
            Some(a) => {
                a.metadata = meta;
                a.is_modified = true;
            }
            None => meta.free(req),
        }
        set?;
        flushed(req, id)
    }

    /// `setSignatureAlgorithm(int $algo, ?string $privateKey = null):
    /// void`.
    #[php(tentative)]
    fn setSignatureAlgorithm(
        req: &mut Request,
        this: This<Self>,
        algo: i64,
        #[php(default = null)] privateKey: Option<PhpString>,
    ) -> Result<(), Thrown> {
        let id = held(req, this.object())?;
        writes_off(
            req,
            id,
            "UnexpectedValueException",
            "Cannot set signature algorithm, phar is read-only",
        )?;
        let Ok(
            algo @ (SIG_SHA256 | SIG_SHA512 | SIG_MD5 | SIG_SHA1 | SIG_OPENSSL | SIG_OPENSSL_SHA256
            | SIG_OPENSSL_SHA512),
        ) = u32::try_from(algo)
        else {
            return Err(throw(
                req,
                "UnexpectedValueException",
                "Unknown signature algorithm specified",
            ));
        };
        if let Some(a) = archives(req).get_mut(id) {
            a.sig_flags = algo;
            a.is_modified = true;
        }
        req.state::<crate::State>().privatekey = privateKey.map(|k| k.as_bytes().to_vec());
        flushed(req, id)
    }

    /// `setStub($stub, int $length = UNKNOWN): true`: a string, or with a
    /// deprecation the contents of a stream.
    #[php(tentative, arity = manual)]
    fn setStub(
        req: &mut Request,
        this: This<Self>,
        stub: Raw<Untyped<Value>>,
        length: Passed<Raw<i64>>,
    ) -> Result<True, Thrown> {
        let id = held(req, this.object())?;
        crate::build::set_stub(req, id, &stub.value(), length)?;
        Ok(True)
    }

    /// `startBuffering(): void`: writes deferred until
    /// `stopBuffering()`.
    #[php(tentative)]
    fn startBuffering(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let id = held(req, this.object())?;
        if let Some(a) = archives(req).get_mut(id) {
            a.donotflush = true;
        }
        Ok(())
    }

    /// `stopBuffering(): void`: the archive written.
    #[php(tentative)]
    fn stopBuffering(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let id = held(req, this.object())?;
        writes_off(
            req,
            id,
            "UnexpectedValueException",
            "Cannot write out phar archive, phar is read-only",
        )?;
        if let Some(a) = archives(req).get_mut(id) {
            a.donotflush = false;
        }
        flushed(req, id)
    }

    /// `Phar::apiVersion(): string`.
    #[php(final)]
    fn apiVersion(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
        PhpString::new(req, archive::API_VERSION.as_bytes())
    }

    /// `Phar::canCompress(int $compression = 0): bool`.
    #[php(final)]
    fn canCompress(req: &mut Request, #[php(default = 0)] compression: i64) -> bool {
        match compression {
            0x1000 => has_zlib(req),
            0x2000 => has_bz2(req),
            _ => has_zlib(req) || has_bz2(req),
        }
    }

    /// `Phar::canWrite(): bool`.
    #[php(final)]
    fn canWrite(req: &mut Request) -> bool {
        !readonly(req)
    }

    /// `Phar::createDefaultStub(?string $index = null, ?string $webIndex =
    /// null): string`.
    #[php(final)]
    fn createDefaultStub(
        req: &mut Request,
        #[php(default = null)] index: Option<PathArg>,
        #[php(default = null)] webIndex: Option<PathArg>,
    ) -> Result<Owner<PhpString>, Thrown> {
        let index = index.map(|s| s.0.as_bytes().to_vec());
        let web = webIndex.map(|s| s.0.as_bytes().to_vec());
        match write::default_stub(index.as_deref(), web.as_deref()) {
            Ok(stub) => PhpString::new(req, &stub),
            Err(error) => Err(throw(req, "PharException", &error)),
        }
    }

    /// `Phar::getSupportedCompression(): array`.
    #[php(final)]
    fn getSupportedCompression(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
        let mut out = PhpArray::new(req)?;
        if has_zlib(req) {
            let v = string(req, b"GZ")?;
            out.append(req, v)?;
        }
        if has_bz2(req) {
            let v = string(req, b"BZIP2")?;
            out.append(req, v)?;
        }
        Ok(out)
    }

    /// `Phar::getSupportedSignatures(): array`.
    #[php(final)]
    fn getSupportedSignatures(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
        let mut out = PhpArray::new(req)?;
        let mut names: Vec<&[u8]> = vec![b"MD5", b"SHA-1", b"SHA-256", b"SHA-512"];
        if req.extension_named(b"openssl").is_some() {
            names.extend([&b"OpenSSL"[..], b"OpenSSL_SHA256", b"OpenSSL_SHA512"]);
        }
        for name in names {
            let v = string(req, name)?;
            out.append(req, v)?;
        }
        Ok(out)
    }

    /// `Phar::interceptFileFuncs(): void`.
    #[php(final)]
    fn interceptFileFuncs(req: &mut Request) {
        crate::intercept::intercept(req);
    }

    /// `Phar::isValidPharFilename(string $filename, bool $executable =
    /// true): bool`.
    #[php(final)]
    fn isValidPharFilename(
        req: &mut Request,
        filename: PathArg,
        #[php(default = true)] executable: bool,
    ) -> Result<bool, Thrown> {
        let exec = if executable {
            Exec::Executable
        } else {
            Exec::Data
        };
        let name = filename.0.as_bytes().to_vec();
        Ok(path::detect_ext(req, &name, exec, Create::Either, true)?.is_ok())
    }

    /// `Phar::loadPhar(string $filename, ?string $alias = null): bool`.
    #[php(final)]
    fn loadPhar(
        req: &mut Request,
        filename: PathArg,
        #[php(default = null)] alias: Option<PhpString>,
    ) -> Result<bool, Thrown> {
        let name = filename.0.as_bytes().to_vec();
        let alias = alias.map(|a| a.as_bytes().to_vec());
        match open::open_from_filename(req, &name, alias.as_deref(), true)? {
            Ok(_) => Ok(true),
            Err(Some(error)) => Err(throw(req, "PharException", &error)),
            Err(None) => Ok(false),
        }
    }

    /// `Phar::mapPhar(?string $alias = null, int $offset = 0): bool`: the
    /// file being served read as an archive.
    #[php(final)]
    fn mapPhar(
        req: &mut Request,
        #[php(default = null)] alias: Option<PhpString>,
        #[php(default = 0)] offset: i64,
    ) -> Result<bool, Thrown> {
        let _ = offset;
        let alias = alias.map(|a| a.as_bytes().to_vec());
        match open::open_executed(req, alias.as_deref())? {
            Ok(()) => Ok(true),
            Err(Some(error)) => Err(throw(req, "PharException", &error)),
            Err(None) => Ok(false),
        }
    }

    /// `Phar::running(bool $returnPhar = true): string`.
    #[php(final)]
    fn running(
        req: &mut Request,
        #[php(default = true)] returnPhar: bool,
    ) -> Result<Owner<PhpString>, Thrown> {
        let found = crate::web::running(req, returnPhar)?;
        PhpString::new(req, &found)
    }

    /// `Phar::mount(string $pharPath, string $externalPath): void`.
    #[php(final)]
    fn mount(req: &mut Request, pharPath: PathArg, externalPath: PathArg) -> Result<(), Thrown> {
        let (inner, outer) = (
            pharPath.0.as_bytes().to_vec(),
            externalPath.0.as_bytes().to_vec(),
        );
        crate::web::mount(req, &inner, &outer)
    }

    /// `Phar::mungServer(array $variables): void`.
    #[php(final)]
    fn mungServer(req: &mut Request, variables: PhpArray) -> Result<(), Thrown> {
        crate::web::mung_server(req, variables)
    }

    /// `Phar::unlinkArchive(string $filename): true`.
    #[php(final)]
    fn unlinkArchive(req: &mut Request, filename: PathArg) -> Result<True, Thrown> {
        let name = filename.0.as_bytes().to_vec();
        crate::build::unlink_archive(req, &name)?;
        Ok(True)
    }

    /// `Phar::webPhar(?string $alias = null, ?string $index = null, ?string
    /// $fileNotFoundScript = null, array $mimeTypes = [], ?callable
    /// $rewrite = null): void`.
    #[php(final)]
    fn webPhar(
        req: &mut Request,
        #[php(default = null)] alias: Option<PhpString>,
        #[php(default = null)] index: Option<PhpString>,
        #[php(default = null)] fileNotFoundScript: Option<PhpString>,
        #[php(default = [])] mimeTypes: PhpArray,
        #[php(default = null)] rewrite: Option<php_ext::abi::Callable>,
    ) -> Result<(), Thrown> {
        crate::web::web_phar(req, alias, index, fileNotFoundScript, mimeTypes, rewrite)
    }
}

/// `class PharData extends RecursiveDirectoryIterator implements
/// Countable, ArrayAccess`: `Phar`'s methods over a non-executable tar or
/// zip archive.
///
/// ```php
/// foreach (['tar' => Phar::TAR, 'zip' => Phar::ZIP] as $ext => $format) {
///     $p = __DIR__ . "/phardata-witness.$ext";
///     $d = new PharData($p);
///     $d['a.txt'] = 'data';
///     $d->addEmptyDir('dir');
///     $d['a.txt']->setMetadata(['k' => 1]);
///     var_dump($d->isFileFormat($format), count($d), $d->getAlias(), $d->getSignature());
///     unset($d);
///     $e = new PharData($p);
///     var_dump($e['a.txt']->getContent(), $e['a.txt']->getMetadata(), is_dir("phar://$p/dir"), $e->getStub());
///     unset($e); unlink($p);
/// }
/// try { new PharData(__DIR__ . '/phardata-witness.phar'); } catch (UnexpectedValueException $e) { echo str_replace(__DIR__, 'D', $e->getMessage()), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(3)
/// NULL
/// bool(false)
/// string(4) "data"
/// array(1) {
///   ["k"]=>
///   int(1)
/// }
/// bool(true)
/// string(0) ""
/// bool(true)
/// int(2)
/// NULL
/// bool(false)
/// string(4) "data"
/// array(1) {
///   ["k"]=>
///   int(1)
/// }
/// bool(true)
/// string(0) ""
/// Cannot create phar 'D/phardata-witness.phar', file extension (or combination) not recognised or the directory does not exist
/// ```
#[php_class(
    extends(RecursiveDirectoryIterator),
    implements(Countable, ArrayAccess)
)]
pub(crate) struct PharData;

#[php_class]
impl PharData {
    /// `PharData::__construct(string $filename, int $flags = SKIP_DOTS |
    /// UNIX_PATHS, ?string $alias = null, int $format = 0)`.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        filename: PathArg,
        #[php(default = FilesystemIterator::SKIP_DOTS|FilesystemIterator::UNIX_PATHS)] flags: i64,
        #[php(default = null)] alias: Option<PhpString>,
        #[php(default = 0)] format: i64,
    ) -> Result<(), Thrown> {
        let alias = alias.map(|a| a.as_bytes().to_vec());
        construct(
            req,
            this.object(),
            filename.0.as_bytes(),
            flags,
            alias.as_deref(),
            format,
        )
    }

    #[php(implementation = Phar::__destruct)]
    fn __destruct(this: This<Self>);
    #[php(tentative, implementation = Phar::addEmptyDir)]
    fn addEmptyDir(this: This<Self>, directory: PathArg);
    #[php(tentative, implementation = Phar::addFile)]
    fn addFile(
        this: This<Self>,
        filename: PathArg,
        #[php(default = null)] localName: Option<PhpString>,
    );
    #[php(tentative, implementation = Phar::addFromString)]
    fn addFromString(this: This<Self>, localName: PathArg, contents: PhpString);
    #[php(tentative, implementation = Phar::buildFromDirectory)]
    fn buildFromDirectory(
        this: This<Self>,
        directory: PathArg,
        #[php(default = "")] pattern: PhpString,
    ) -> PhpArray;
    #[php(tentative, implementation = Phar::buildFromIterator)]
    fn buildFromIterator(
        this: This<Self>,
        iterator: php_ext::abi::Instance<Traversable>,
        #[php(default = null)] baseDirectory: Option<PhpString>,
    ) -> PhpArray;
    #[php(tentative, implementation = Phar::compressFiles)]
    fn compressFiles(this: This<Self>, compression: i64);
    #[php(tentative, implementation = Phar::decompressFiles)]
    fn decompressFiles(this: This<Self>) -> True;
    #[php(tentative, implementation = Phar::compress)]
    fn compress(
        this: This<Self>,
        compression: i64,
        #[php(default = null)] extension: Option<PhpString>,
    ) -> Option<php_ext::abi::Instance<PharData>>;
    #[php(tentative, implementation = Phar::decompress)]
    fn decompress(
        this: This<Self>,
        #[php(default = null)] extension: Option<PhpString>,
    ) -> Option<php_ext::abi::Instance<PharData>>;
    #[php(tentative, implementation = Phar::convertToExecutable)]
    fn convertToExecutable(
        this: This<Self>,
        #[php(default = null)] format: Option<i64>,
        #[php(default = null)] compression: Option<i64>,
        #[php(default = null)] extension: Option<PhpString>,
    ) -> Option<php_ext::abi::Instance<Phar>>;
    #[php(tentative, implementation = Phar::convertToData)]
    fn convertToData(
        this: This<Self>,
        #[php(default = null)] format: Option<i64>,
        #[php(default = null)] compression: Option<i64>,
        #[php(default = null)] extension: Option<PhpString>,
    ) -> Option<php_ext::abi::Instance<PharData>>;
    #[php(tentative, implementation = Phar::copy)]
    fn copy(this: This<Self>, from: PathArg, to: PathArg) -> True;
    #[php(tentative, implementation = Phar::count)]
    fn count(this: This<Self>, #[php(default = COUNT_NORMAL)] mode: i64) -> i64;
    #[php(tentative, implementation = Phar::delete)]
    fn delete(this: This<Self>, localName: PathArg) -> True;
    #[php(tentative, implementation = Phar::delMetadata)]
    fn delMetadata(this: This<Self>) -> True;
    #[php(tentative, implementation = Phar::extractTo)]
    fn extractTo(
        this: This<Self>,
        directory: PathArg,
        #[php(default = null)] files: Option<Union<PhpArray, PhpString>>,
        #[php(default = false)] overwrite: bool,
    ) -> bool;
    #[php(tentative, implementation = Phar::getAlias)]
    fn getAlias(this: This<Self>) -> Option<PhpString>;
    #[php(tentative, implementation = Phar::getPath)]
    fn getPath(this: This<Self>) -> PhpString;
    #[php(tentative, implementation = Phar::getMetadata)]
    fn getMetadata(
        this: This<Self>,
        #[php(default = [])] unserializeOptions: Passed<PhpArray>,
    ) -> Value;
    #[php(tentative, implementation = Phar::getModified)]
    fn getModified(this: This<Self>) -> bool;
    #[php(tentative, implementation = Phar::getSignature)]
    fn getSignature(this: This<Self>) -> OrFalse<PhpArray>;
    #[php(tentative, implementation = Phar::getStub)]
    fn getStub(this: This<Self>) -> PhpString;
    #[php(tentative, implementation = Phar::getVersion)]
    fn getVersion(this: This<Self>) -> PhpString;
    #[php(tentative, implementation = Phar::hasMetadata)]
    fn hasMetadata(this: This<Self>) -> bool;
    #[php(tentative, implementation = Phar::isBuffering)]
    fn isBuffering(this: This<Self>) -> bool;
    #[php(tentative, implementation = Phar::isCompressed)]
    fn isCompressed(this: This<Self>) -> OrFalse<i64>;
    #[php(tentative, implementation = Phar::isFileFormat)]
    fn isFileFormat(this: This<Self>, format: i64) -> bool;
    #[php(tentative, implementation = Phar::isWritable)]
    fn isWritable(this: This<Self>) -> bool;
    #[php(tentative, implementation = Phar::offsetExists)]
    fn offsetExists(this: This<Self>, localName: Untyped<PathArg>) -> bool;
    #[php(tentative, implementation = Phar::offsetGet)]
    fn offsetGet(
        this: This<Self>,
        localName: Untyped<PathArg>,
    ) -> php_ext::abi::Instance<SplFileInfo>;
    #[php(tentative, arity = manual, implementation = Phar::offsetSet)]
    fn offsetSet(this: This<Self>, localName: Raw<Untyped<Value>>, value: Raw<Untyped<Value>>);
    #[php(tentative, implementation = Phar::offsetUnset)]
    fn offsetUnset(this: This<Self>, localName: Untyped<PathArg>);
    #[php(tentative, implementation = Phar::setAlias)]
    fn setAlias(this: This<Self>, alias: PhpString) -> bool;
    #[php(tentative, implementation = Phar::setDefaultStub)]
    fn setDefaultStub(
        this: This<Self>,
        #[php(default = null)] index: Option<PhpString>,
        #[php(default = null)] webIndex: Option<PhpString>,
    ) -> bool;
    #[php(tentative, implementation = Phar::setMetadata)]
    fn setMetadata(this: This<Self>, metadata: Value);
    #[php(tentative, implementation = Phar::setSignatureAlgorithm)]
    fn setSignatureAlgorithm(
        this: This<Self>,
        algo: i64,
        #[php(default = null)] privateKey: Option<PhpString>,
    );
    #[php(arity = manual, implementation = Phar::setStub)]
    fn setStub(this: This<Self>, stub: Raw<Untyped<Value>>, length: Passed<Raw<i64>>) -> True;
    #[php(tentative, implementation = Phar::startBuffering)]
    fn startBuffering(this: This<Self>);
    #[php(tentative, implementation = Phar::stopBuffering)]
    fn stopBuffering(this: This<Self>);
    #[php(final, implementation = Phar::apiVersion)]
    fn apiVersion() -> PhpString;
    #[php(final, implementation = Phar::canCompress)]
    fn canCompress(#[php(default = 0)] compression: i64) -> bool;
    #[php(final, implementation = Phar::canWrite)]
    fn canWrite() -> bool;
    #[php(final, implementation = Phar::createDefaultStub)]
    fn createDefaultStub(
        #[php(default = null)] index: Option<PathArg>,
        #[php(default = null)] webIndex: Option<PathArg>,
    ) -> PhpString;
    #[php(final, implementation = Phar::getSupportedCompression)]
    fn getSupportedCompression() -> PhpArray;
    #[php(final, implementation = Phar::getSupportedSignatures)]
    fn getSupportedSignatures() -> PhpArray;
    #[php(final, implementation = Phar::interceptFileFuncs)]
    fn interceptFileFuncs();
    #[php(final, implementation = Phar::isValidPharFilename)]
    fn isValidPharFilename(filename: PathArg, #[php(default = true)] executable: bool) -> bool;
    #[php(final, implementation = Phar::loadPhar)]
    fn loadPhar(filename: PathArg, #[php(default = null)] alias: Option<PhpString>) -> bool;
    #[php(final, implementation = Phar::mapPhar)]
    fn mapPhar(
        #[php(default = null)] alias: Option<PhpString>,
        #[php(default = 0)] offset: i64,
    ) -> bool;
    #[php(final, implementation = Phar::running)]
    fn running(#[php(default = true)] returnPhar: bool) -> PhpString;
    #[php(final, implementation = Phar::mount)]
    fn mount(pharPath: PathArg, externalPath: PathArg);
    #[php(final, implementation = Phar::mungServer)]
    fn mungServer(variables: PhpArray);
    #[php(final, implementation = Phar::unlinkArchive)]
    fn unlinkArchive(filename: PathArg) -> True;
    #[php(final, implementation = Phar::webPhar)]
    fn webPhar(
        #[php(default = null)] alias: Option<PhpString>,
        #[php(default = null)] index: Option<PhpString>,
        #[php(default = null)] fileNotFoundScript: Option<PhpString>,
        #[php(default = [])] mimeTypes: PhpArray,
        #[php(default = null)] rewrite: Option<php_ext::abi::Callable>,
    );
}

/// `phar_test_compression` over the manifest: no live entry compressed by
/// a library that is not loaded.
fn can_decompress_all(req: &mut Request, id: ArchiveId) -> bool {
    let (zlib, bz2) = (has_zlib(req), has_bz2(req));
    archives(req).get(id).is_none_or(|a| {
        a.manifest.iter().all(|(_, e)| {
            e.is_deleted
                || ((bz2 || e.flags & ENT_COMPRESSED_BZ2 == 0)
                    && (zlib || e.flags & ENT_COMPRESSED_GZ == 0))
        })
    })
}

/// `pharobj_set_compression`: every live entry's compression bits set,
/// the archive marked modified.
fn set_compression(req: &mut Request, id: ArchiveId, flags: u32) {
    let Some(a) = archives(req).get_mut(id) else {
        return;
    };
    for slot in a.manifest.slots() {
        if let Some(e) = a.manifest.at_mut(slot)
            && !e.is_deleted
        {
            e.old_flags = e.flags;
            e.flags = (e.flags & !ENT_COMPRESSION_MASK) | flags;
            e.is_modified = true;
        }
    }
    a.is_modified = true;
}

/// `phar_metadata_tracker_unserialize_or_copy`: the value kept, or the
/// string unserialised under the options, `null` where there is nothing.
pub(crate) fn metadata_value(
    req: &mut Request,
    meta: &mut Metadata,
    options: Passed<PhpArray>,
    function: &str,
) -> Result<Owner<Value>, Thrown> {
    if !meta.has_data() {
        return Ok(Owner::null());
    }
    let options = match options {
        Passed::Given(options) if !options.is_empty() => Some(options),
        _ => None,
    };
    if meta.val.is_none() || options.is_some() {
        let data = meta.str.clone().unwrap_or_default();
        return php_ext_standard::php_unserialize_with_options(req, &data, options, function);
    }
    Ok(Owner::take(
        req,
        meta.val.as_deref().copied().unwrap_or_default(),
    ))
}

/// `serialize_metadata_or_throw`: the value serialised, then kept with
/// its string, the old ones given up.
pub(crate) fn set_metadata(
    req: &mut Request,
    meta: &mut Metadata,
    value: &Value,
) -> Result<(), Thrown> {
    let mut out = Vec::new();
    let mut serializer = php_ext_standard::Serializer::new(req);
    let written = serializer.write(req, &mut out, value);
    serializer.finish(req);
    written?;
    meta.free(req);
    meta.val = Some(Owner::take(req, value.deref()));
    meta.str = Some(out);
    Ok(())
}

/// What `offsetSet()`'s two parses read: a resource's stream or a string.
enum NameValue {
    Stream(Stream),
    Bytes(Vec<u8>),
}

/// `offsetSet`'s `zend_parse_parameters_ex(QUIET, "Pr")` then `"PS"`: the
/// name as a path and the value a stream, or both strings, php's
/// `TypeError` naming the failed second parse.
fn zpp_name_and_value(
    req: &mut Request,
    name: Value,
    value: Value,
) -> Result<(Vec<u8>, NameValue), Thrown> {
    arity(req, 2, Some(2))?;
    let name = name.deref();
    let value = value.deref();
    let name_bytes = match name.as_ref() {
        ValueRef::Str(s) => Some(s.as_bytes().to_vec()),
        ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Bool(_) => {
            Some(with_string(req, &name, <[u8]>::to_vec)?)
        }
        _ => None,
    };
    if let (Some(n), Some(stream)) = (&name_bytes, Stream::from_value(req, &value))
        && !n.contains(&0)
    {
        return Ok((n.clone(), NameValue::Stream(stream)));
    }
    let Some(n) = name_bytes else {
        let given = value_name(req, &name);
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type string, {given} given"),
        ));
    };
    if n.contains(&0) {
        return Err(arg_value_error(req, 1, "not contain any null bytes"));
    }
    let bytes = match value.as_ref() {
        ValueRef::Str(s) => s.as_bytes().to_vec(),
        ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Bool(_) => {
            with_string(req, &value, <[u8]>::to_vec)?
        }
        _ => {
            let given = value_name(req, &value);
            return Err(arg_type_error(
                req,
                2,
                &format!("be of type string, {given} given"),
            ));
        }
    };
    Ok((n, NameValue::Bytes(bytes)))
}

/// `instanceof_function` for two classes, over the parents alone, which
/// is all a class extending `PharFileInfo` can reach it by.
fn extends(req: &mut Request, class: ClassId, of: ClassId) -> bool {
    req.lineage(class).any(|class| class == of)
}
