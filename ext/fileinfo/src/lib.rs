//! The `fileinfo` extension: `php-src/ext/fileinfo` as one crate under
//! `ext/` (ADR-0004), as php's default `./configure` builds it.
//! `fileinfo.c` ported over php-src's vendored libmagic, which `build.rs`
//! compiles with its compiled-in database and [`ffi`] binds (design
//! §7.2, #1451). One dependency beside the facade, libpcre2-8, which
//! libmagic's `regex` entries are matched with as php's ext/pcre matches
//! them; one entry, `extension()`; no state (ADR-0005). The phpt subset is
//! `php-src/ext/fileinfo/tests`, through `xtask phpt --subset`.
//!
//! A file is read through the engine's streams, its wrapper and
//! `open_basedir` included, and handed to libmagic with its stat and the
//! fd under the stream, as `magic_stream` reads php's stream.
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod ffi;

use std::ffi::CString;

use php_ext::abi::{Extension, ExtensionBuilder, Instance, OrFalse, Path, Raw, True, Untyped};
use php_ext::attr::{php_const, php_function};
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::value_name;
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, docref, docref_parameter, pending, throw,
};
use php_ext::request::{
    Cast, Opening, OwnedStream as _, Request, Resolve, Stream, StreamCtx, Streams as _, Whence,
};
use php_ext::values::{Owner, PhpObject, PhpString, Resource, Value, ValueRef};

use crate::ffi::{Magic, Read};

/// The extension value (design §7.1): the six functions, the class and
/// its constants.
///
/// ```php
/// var_dump(extension_loaded('fileinfo'), FILEINFO_MIME, FILEINFO_EXTENSION);
/// ```
/// ```expect
/// bool(true)
/// int(1040)
/// int(16777216)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            finfo_open::declared(),
            finfo_close::declared(),
            finfo_set_flags::declared(),
            finfo_file::declared(),
            finfo_buffer::declared(),
            mime_content_type::declared(),
        ])
        .constants(constants::declared())
        .class::<class::finfo>()
        .info(fileinfo_info)
        .build("fileinfo")
}

/// `fileinfo.stub.php`'s constants, libmagic's `MAGIC_*` flags.
///
/// ```php
/// var_dump(FILEINFO_NONE, FILEINFO_SYMLINK, FILEINFO_MIME_TYPE, FILEINFO_MIME_ENCODING, FILEINFO_DEVICES, FILEINFO_CONTINUE, FILEINFO_PRESERVE_ATIME, FILEINFO_RAW, FILEINFO_APPLE);
/// ```
/// ```expect
/// int(0)
/// int(2)
/// int(16)
/// int(1024)
/// int(8)
/// int(32)
/// int(128)
/// int(256)
/// int(2048)
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const FILEINFO_NONE: i64 = 0;
    pub(crate) const FILEINFO_SYMLINK: i64 = 0x2;
    pub(crate) const FILEINFO_MIME: i64 = 0x410;
    pub(crate) const FILEINFO_MIME_TYPE: i64 = 0x10;
    pub(crate) const FILEINFO_MIME_ENCODING: i64 = 0x400;
    pub(crate) const FILEINFO_DEVICES: i64 = 0x8;
    pub(crate) const FILEINFO_CONTINUE: i64 = 0x20;
    pub(crate) const FILEINFO_PRESERVE_ATIME: i64 = 0x80;
    pub(crate) const FILEINFO_RAW: i64 = 0x100;
    pub(crate) const FILEINFO_APPLE: i64 = 0x800;
    pub(crate) const FILEINFO_EXTENSION: i64 = 0x0100_0000;
}

/// `PHP_MINFO_FUNCTION(fileinfo)`: libmagic's version as `major.minor`.
///
/// ```php
/// (new ReflectionExtension('fileinfo'))->info();
/// ```
/// ```expect
///
/// fileinfo
///
/// fileinfo support => enabled
/// libmagic => 5.46
/// ```
fn fileinfo_info(req: &mut Request) {
    let version = ffi::version();
    let text = format!("{}.{}", version / 100, version % 100);
    req.extensions_mut()
        .info_table(&[&["fileinfo support", "enabled"], &["libmagic", &text]]);
}

/// The C `int` php hands libmagic a `zend_long` as.
#[expect(clippy::cast_possible_truncation, reason = "php's `(int) options`")]
const fn c_int(value: i64) -> i32 {
    value as i32
}

/// Every diagnostic libmagic raised through php's `php_error_docref`
/// during the call that just returned, raised now in order.
fn raise_kept(req: &mut Request) {
    while let Some((param, message)) = ffi::take_warning() {
        match param {
            Some(param) => docref_parameter(req, Level::Warning, &param, &message),
            None => docref(req, Level::Warning, &message),
        }
    }
}

/// The class, in a `mod` of its own so `finfo` names the parameter of
/// the functions and the class only as `class::finfo`.
mod class {
    use php_ext::abi::{OrFalse, Path, Refusals, This, True};
    use php_ext::attr::php_class;
    use php_ext::raising::{Thrown, throw, throwing_warnings};
    use php_ext::request::Request;
    use php_ext::values::{Owner, PhpString, Resource};

    use crate::ffi::Magic;
    use crate::{buffer, file, open, set_flags};

    /// `class finfo`, `@not-serializable`: one `finfo_object`, 64 bytes, its
    /// payload the libmagic set, `None` before a constructor made one.
    ///
    /// ```php
    /// $f = new finfo;
    /// try { clone $f; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { serialize($f); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// $g = (new ReflectionClass('finfo'))->newInstanceWithoutConstructor();
    /// try { $g->buffer('x'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($f->buffer("<?php\n"));
    /// ```
    /// ```expect
    /// Trying to clone an uncloneable object of class finfo
    /// Serialization of 'finfo' is not allowed
    /// Invalid finfo object
    /// string(%d) "PHP script, ASCII text"
    /// ```
    #[allow(non_camel_case_types)]
    #[php_class(not_serializable, size = 64, payload = Option<Magic>, handlers = FINFO)]
    pub(crate) struct finfo;

    #[php_class]
    impl finfo {
        /// `finfo::__construct(int $flags = FILEINFO_NONE, ?string
        /// $magic_database = null)`, `finfo_open` on the object: warnings
        /// thrown as `Exception`, and `Constructor failed` where nothing else
        /// was thrown.
        ///
        /// ```php
        /// try { new finfo(FILEINFO_NONE, '/foo/bar/inexistent'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
        /// $f = new finfo(FILEINFO_MIME_TYPE); $f->__construct(); var_dump($f->buffer("GIF89a"));
        /// ```
        /// ```expect
        /// finfo::__construct(/foo/bar/inexistent): Failed to open stream: No such file or directory
        /// string(%d) "%s"
        /// ```
        fn __construct(
            req: &mut Request,
            this: This<Self>,
            #[php(default = FILEINFO_NONE)] flags: i64,
            #[php(default = null)] magic_database: Option<Path>,
        ) -> Result<(), Thrown> {
            *this.payload_mut(req)? = None;
            let opened =
                throwing_warnings(req, "Exception", |req| open(req, flags, magic_database))?;
            if opened.is_none() {
                return Err(throw(req, "Exception", "Constructor failed"));
            }
            *this.payload_mut(req)? = opened;
            Ok(())
        }

        /// `finfo::file(string $filename, int $flags, $context): string|false`,
        /// `finfo_file` on the object.
        ///
        /// ```php
        /// var_dump((new finfo(FILEINFO_MIME_TYPE))->file(__FILE__));
        /// ```
        /// ```expect
        /// string(10) "text/x-php"
        /// ```
        #[php(tentative)]
        fn file(
            req: &mut Request,
            this: This<Self>,
            filename: Path,
            #[php(default = FILEINFO_NONE)] flags: i64,
            #[php(default = null)] context: Option<Resource>,
        ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
            file(req, this.object(), filename, flags, context)
        }

        /// `finfo::buffer(string $string, int $flags, $context): string|false`,
        /// `finfo_buffer` on the object.
        ///
        /// ```php
        /// var_dump((new finfo(FILEINFO_MIME))->buffer("%PDF-1.4", FILEINFO_MIME_TYPE, null));
        /// ```
        /// ```expect
        /// Deprecated: finfo::buffer(): The $context parameter has no effect for finfo_buffer() in %s on line 2
        /// string(15) "application/pdf"
        /// ```
        #[php(tentative)]
        fn buffer(
            req: &mut Request,
            this: This<Self>,
            string: PhpString,
            #[php(default = FILEINFO_NONE)] flags: i64,
            #[php(default = null)] context: Option<Resource>,
        ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
            let _ = context;
            let deprecated = req.argument_count() == 3;
            buffer(req, this.object(), string, flags, deprecated)
        }

        /// `finfo::set_flags(int $flags): true`, `finfo_set_flags` on the
        /// object.
        ///
        /// ```php
        /// $f = new finfo; var_dump($f->set_flags(FILEINFO_MIME_TYPE), $f->buffer("\x89PNG\r\n\x1a\n"));
        /// ```
        /// ```expect
        /// bool(true)
        /// string(24) "application/octet-stream"
        /// ```
        #[php(tentative)]
        fn set_flags(req: &mut Request, this: This<Self>, flags: i64) -> Result<True, Thrown> {
            set_flags(req, this.object(), flags)
        }
    }

    /// `finfo_object_handlers`: `std_object_handlers` with php's null
    /// `clone_obj`; `free_obj`'s `magic_close` is the payload's drop.
    pub(crate) const FINFO: Refusals = Refusals::NONE.cloning();
}

/// The set an object holds, taken out for `f`, or php's `Invalid finfo
/// object` for none.
fn with_magic<R>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut Request, &mut Magic) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let Some(mut magic) = object.native_mut_or_fatal::<Option<Magic>>(req)?.take() else {
        return Err(throw(req, "Error", "Invalid finfo object"));
    };
    let answer = f(req, &mut magic);
    let slot = object.native_mut_or_fatal::<Option<Magic>>(req)?;
    if slot.is_none() {
        *slot = Some(magic);
    }
    answer
}

/// `finfo_open`'s body past its arguments: the database's path checked
/// against `open_basedir` and expanded, the set opened and the database
/// loaded; `None` after php's warning for what failed.
fn open(
    req: &mut Request,
    flags: i64,
    magic_database: Option<Path>,
) -> Result<Option<Magic>, Thrown> {
    let file = match magic_database.map(|path| path.0.as_bytes().to_vec()) {
        Some(file) if !file.is_empty() => {
            if req
                .environment_mut()
                .check_open_basedir(&file, true)
                .is_err()
            {
                return Ok(None);
            }
            let cwd = req
                .environment()
                .cwd()
                .into_os_string()
                .into_encoded_bytes();
            let Some(resolved) =
                req.environment_mut()
                    .virtual_file_ex(&cwd, &file, Resolve::Expand)
            else {
                return Ok(None);
            };
            Some(resolved)
        }
        _ => None,
    };
    let Some(mut magic) = Magic::open(c_int(flags)) else {
        docref(req, Level::Warning, &format!("Invalid mode '{flags}'."));
        return Ok(None);
    };
    let path = file
        .as_ref()
        .and_then(|file| CString::new(file.clone()).ok());
    let loaded = magic.load(path.as_deref());
    raise_kept(req);
    pending(req)?;
    if !loaded {
        let shown = file.as_deref().map_or_else(
            || String::from("(null)"),
            |f| String::from_utf8_lossy(f).into_owned(),
        );
        let text = format!("Failed to load magic database at \"{shown}\"");
        docref(req, Level::Warning, &text);
        return Ok(None);
    }
    Ok(Some(magic))
}

/// `finfo_open(int $flags = FILEINFO_NONE, ?string $magic_database =
/// null): finfo|false`: a new `finfo` over the database at the path, or
/// the one libmagic was built with.
///
/// ```php
/// var_dump(finfo_open(FILEINFO_MIME, ''), finfo_open(FILEINFO_MIME, '/foo/bar/inexistent'));
/// try { finfo_open(FILEINFO_MIME, "\0"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: finfo_open(/foo/bar/inexistent): Failed to open stream: No such file or directory in %s on line 2
///
/// Warning: finfo_open(/foo/bar/inexistent): Failed to open stream: No such file or directory in %s on line 2
///
/// Warning: finfo_open(): Failed to load magic database at "/foo/bar/inexistent" in %s on line 2
/// object(finfo)#1 (0) {
/// }
/// bool(false)
/// finfo_open(): Argument #2 ($magic_database) must not contain any null bytes
/// ```
#[php_function]
fn finfo_open(
    req: &mut Request,
    #[php(default = FILEINFO_NONE)] flags: i64,
    #[php(default = null)] magic_database: Option<Path>,
) -> Result<OrFalse<Owner<Instance<class::finfo>>>, Thrown> {
    let Some(magic) = open(req, flags, magic_database)? else {
        return Ok(OrFalse::False);
    };
    let Some(class) = lookup_class(req, b"finfo")? else {
        return Err(throw(req, "Error", "Class \"finfo\" not found"));
    };
    let object = new_object(req, class)?.into_instance::<class::finfo>();
    *object.payload_mut(req)? = Some(magic);
    Ok(OrFalse::Value(object))
}

/// `finfo_close(finfo $finfo): true`, deprecated in 8.5: nothing, since
/// the object closes its set when it is freed.
///
/// ```php
/// var_dump(finfo_close(finfo_open()));
/// ```
/// ```expect
/// Deprecated: Function finfo_close() is deprecated since 8.5, as finfo objects are freed automatically in %s on line 2
/// bool(true)
/// ```
#[php_function(deprecated(since = "8.5", message = "as finfo objects are freed automatically"))]
fn finfo_close(req: &mut Request, finfo: Instance<class::finfo>) -> True {
    let _ = (req, finfo);
    True
}

/// `finfo_set_flags`'s body: the set's flags, what the object answers
/// with until they are set again.
fn set_flags(req: &mut Request, object: PhpObject, flags: i64) -> Result<True, Thrown> {
    with_magic(req, object, |_, magic| {
        magic.set_flags(c_int(flags));
        Ok(True)
    })
}

/// `finfo_set_flags(finfo $finfo, int $flags): true`.
///
/// ```php
/// $f = finfo_open(); var_dump(finfo_set_flags($f, FILEINFO_MIME), finfo_buffer($f, "GIF89a"));
/// ```
/// ```expect
/// bool(true)
/// string(27) "image/gif; charset=us-ascii"
/// ```
#[php_function]
fn finfo_set_flags(
    req: &mut Request,
    finfo: Instance<class::finfo>,
    flags: i64,
) -> Result<True, Thrown> {
    set_flags(req, finfo.object(), flags)
}

/// `S_IFMT` and `S_IFDIR`.
const S_IFMT: i64 = 0o170_000;
const S_IFDIR: i64 = 0o040_000;

/// `magic_stream` over an open stream from where it stands: up to the
/// set's `bytes_max` bytes read, with its stat's `st_mode` and `st_size`
/// and the fd under it; php's `Failed identify data` where libmagic
/// answers nothing.
fn identify_stream(req: &mut Request, magic: &mut Magic, stream: Stream) -> Option<Vec<u8>> {
    let stat = stream.stat(req).unwrap_or_default();
    let mut bytes = vec![0u8; magic.bytes_max()];
    let mut had = 0;
    while had < bytes.len() {
        match stream.read_into(req, &mut bytes[had..]) {
            Some(0) | None => break,
            Some(n) => had += n,
        }
    }
    let fd = stream.cast(req, Cast::Internal, false);
    let read = Read {
        bytes: &bytes[..had],
        fd,
        mode: u32::try_from(stat[2]).unwrap_or(0),
        size: stat[7],
    };
    let answer = magic.stream(&read);
    raise_kept(req);
    if answer.is_none() {
        let (errno, error) = magic.error();
        docref(
            req,
            Level::Warning,
            &format!("Failed identify data {errno}:{error}"),
        );
    }
    answer
}

/// `php_fileinfo_from_path`: the path opened through its wrapper under
/// `$context`, `directory` for a directory, otherwise what libmagic
/// finds in it.
fn from_path(
    req: &mut Request,
    magic: &mut Magic,
    path: &[u8],
    context: Option<StreamCtx>,
) -> Result<Option<Vec<u8>>, Thrown> {
    let opening = Opening {
        warn: true,
        ..Default::default()
    };
    let opened = req.with_stream_ctx(context, |req| req.open_stream_with(path, b"rb", opening))?;
    let Some(stream) = opened else {
        return Ok(None);
    };
    let answer = match stream.stat(req) {
        Some(stat) if stat[2] & S_IFMT == S_IFDIR => Some(b"directory".to_vec()),
        Some(_) => identify_stream(req, magic, *stream),
        None => None,
    };
    stream.free(req);
    Ok(answer)
}

/// `finfo_file`'s body, the function's and the method's.
fn file(
    req: &mut Request,
    object: PhpObject,
    filename: Path,
    flags: i64,
    context: Option<Resource>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let path = filename.0.as_bytes().to_vec();
    let answer = with_magic(req, object, |req, magic| {
        if path.is_empty() {
            return Err(arg_value_error(req, 2, "not be empty"));
        }
        // `php_stream_context_from_zval`'s fetch of a `$context` given.
        let context = context.map(|res| StreamCtx::fetch(req, res)).transpose()?;
        let old = magic.flags();
        if flags != 0 {
            magic.set_flags(c_int(flags));
        }
        let answer = from_path(req, magic, &path, context);
        if flags != 0 {
            magic.set_flags(old);
        }
        answer
    })?;
    match answer {
        Some(text) => Ok(OrFalse::Value(PhpString::new(req, &text)?)),
        None => Ok(OrFalse::False),
    }
}

/// `finfo_file(finfo $finfo, string $filename, int $flags, $context)`:
/// what libmagic finds in the file, with the flags for this call alone,
/// or `false`.
///
/// ```php
/// $f = finfo_open();
/// var_dump(finfo_file($f, '.'), finfo_file($f, __FILE__, FILEINFO_MIME_TYPE), finfo_file($f, '&'));
/// try { finfo_file($f, ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: finfo_file(&): Failed to open stream: No such file or directory in %s on line 3
/// string(9) "directory"
/// string(10) "text/x-php"
/// bool(false)
/// finfo_file(): Argument #2 ($filename) must not be empty
/// ```
#[php_function]
fn finfo_file(
    req: &mut Request,
    finfo: Instance<class::finfo>,
    filename: Path,
    #[php(default = FILEINFO_NONE)] flags: i64,
    #[php(default = null)] context: Option<Resource>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    file(req, finfo.object(), filename, flags, context)
}

/// `finfo_buffer`'s body, the function's and the method's: the
/// deprecation of `$context` first, then what libmagic finds in the
/// bytes.
fn buffer(
    req: &mut Request,
    object: PhpObject,
    string: PhpString,
    flags: i64,
    deprecated: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    if deprecated {
        let text = "The $context parameter has no effect for finfo_buffer()";
        docref(req, Level::Deprecated, text);
        pending(req)?;
    }
    let answer = with_magic(req, object, |req, magic| {
        let old = magic.flags();
        if flags != 0 {
            magic.set_flags(c_int(flags));
        }
        let answer = magic.buffer(string.as_bytes());
        let error = magic.error();
        if flags != 0 {
            magic.set_flags(old);
        }
        raise_kept(req);
        Ok(answer.ok_or(error))
    })?;
    match answer {
        Ok(text) => Ok(OrFalse::Value(PhpString::new(req, &text)?)),
        Err((errno, error)) => {
            docref(
                req,
                Level::Warning,
                &format!("Failed identify data {errno}:{error}"),
            );
            Ok(OrFalse::False)
        }
    }
}

/// `finfo_buffer(finfo $finfo, string $string, int $flags, $context)`:
/// what libmagic finds in the bytes, or `false`.
///
/// ```php
/// $f = finfo_open(FILEINFO_MIME);
/// var_dump(finfo_buffer($f, "Regular string here"), finfo_buffer($f, ""), finfo_buffer($f, "x", FILEINFO_NONE, null));
/// ```
/// ```expect
/// Deprecated: finfo_buffer(): The $context parameter has no effect for finfo_buffer() in %s on line 3
/// string(28) "text/plain; charset=us-ascii"
/// string(%d) "%s"
/// string(%d) "%s"
/// ```
#[php_function]
fn finfo_buffer(
    req: &mut Request,
    finfo: Instance<class::finfo>,
    string: PhpString,
    #[php(default = FILEINFO_NONE)] flags: i64,
    #[php(default = null)] context: Option<Resource>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let _ = context;
    let deprecated = req.argument_count() == 4;
    buffer(req, finfo.object(), string, flags, deprecated)
}

/// What `mime_content_type` was handed: a path, or an open stream.
enum Target {
    Path(Vec<u8>),
    Stream(Stream),
}

/// `mime_content_type`'s argument as its `zend_parse_parameters("z")` and
/// switch read it.
fn target(req: &mut Request, value: Value) -> Result<Target, Thrown> {
    match value.as_ref() {
        ValueRef::Str(path) => {
            let path = path.as_bytes().to_vec();
            if path.is_empty() {
                return Err(arg_value_error(req, 1, "not be empty"));
            }
            if path.contains(&0) {
                return Err(arg_type_error(req, 1, "not contain any null bytes"));
            }
            Ok(Target::Path(path))
        }
        ValueRef::Resource(_) => match Stream::from_value(req, &value) {
            Some(stream) => Ok(Target::Stream(stream)),
            None => Err(throw(
                req,
                "TypeError",
                "mime_content_type(): supplied resource is not a valid stream resource",
            )),
        },
        _ => {
            let name = value_name(req, &value);
            let tail = format!("be of type resource|string, {name} given");
            Err(arg_type_error(req, 1, &tail))
        }
    }
}

/// `mime_content_type($filename): string|false`: the MIME type of a file
/// by its path, or of an open stream read from its start and left where
/// it stood.
///
/// ```php
/// var_dump(mime_content_type(__FILE__));
/// $s = fopen(__FILE__, 'r'); fread($s, 3);
/// var_dump(mime_content_type($s), ftell($s));
/// foreach ([1, null, ''] as $v) { try { mime_content_type($v); } catch (Error $e) { echo $e->getMessage(), "\n"; } }
/// try { mime_content_type("\0"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(10) "text/x-php"
/// string(10) "text/x-php"
/// int(3)
/// mime_content_type(): Argument #1 ($filename) must be of type resource|string, int given
/// mime_content_type(): Argument #1 ($filename) must be of type resource|string, null given
/// mime_content_type(): Argument #1 ($filename) must not be empty
/// mime_content_type(): Argument #1 ($filename) must not contain any null bytes
/// ```
#[php_function]
fn mime_content_type(
    req: &mut Request,
    filename: Raw<Untyped<Value>>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let target = target(req, filename.value().deref())?;
    // `MAGIC_MIME_TYPE`.
    let Some(mut magic) = Magic::open(0x10) else {
        docref(req, Level::Warning, "Failed to load magic database");
        return Ok(OrFalse::False);
    };
    if !magic.load(None) {
        raise_kept(req);
        let (errno, error) = magic.error();
        docref(
            req,
            Level::Warning,
            &format!("Failed identify data {errno}:{error}"),
        );
        return Ok(OrFalse::False);
    }
    let answer = match target {
        Target::Path(path) => from_path(req, &mut magic, &path, None)?,
        Target::Stream(stream) => {
            let at = stream.tell(req);
            stream.seek(req, 0, Whence::Set);
            let answer = identify_stream(req, &mut magic, stream);
            stream.seek(req, at, Whence::Set);
            answer
        }
    };
    match answer {
        Some(text) => Ok(OrFalse::Value(PhpString::new(req, &text)?)),
        None => Ok(OrFalse::False),
    }
}
