//! `curl_file.c`: the `CURLFile` and `CURLStringFile` classes and
//! `curl_file_create`; with `build_mime_structure_from_hash`, which turns
//! a `CURLOPT_POSTFIELDS` array into a MIME form, and the file a
//! `CURLFile` part reads while the transfer runs.

use std::ops::ControlFlow;

use php_ext::abi::{Instance, Path, This};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{lookup_class, read_prop, write_prop};
use php_ext::coerce::{to_string, with_string};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Opening, OwnedStream as _, Request, Stream, Streams as _, Whence};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::ffi::MimeBuilder;
use crate::handle::{Curl, easy_of, made};

/// `struct mime_data_cb_arg`: the path a `CURLFile` part reads and the
/// stream open on it, opened again where a transfer finds it closed, an
/// owner the part holds until its `free_cb`.
pub(crate) struct Source {
    filename: Vec<u8>,
    stream: Option<Owner<Stream>>,
}

impl Source {
    pub(crate) fn is_open(&self) -> bool {
        self.stream.is_some()
    }

    /// `read_cb`: up to `buffer`'s length read, the stream opened first
    /// where it is not; `None` for php's `CURL_READFUNC_ABORT`.
    pub(crate) fn read(&mut self, req: &mut Request, buffer: &mut [u8]) -> Option<usize> {
        let stream: Stream = if let Some(stream) = self.stream.as_deref() {
            *stream
        } else {
            let opened = req
                .open_stream_with(&self.filename, b"rb", Opening::default())
                .ok()??;
            let view = *opened;
            self.stream = Some(opened);
            view
        };
        if let Ok(Some(read)) = stream.read(req, buffer.len()) {
            let bytes = read.as_bytes();
            let len = bytes.len().min(buffer.len());
            buffer[..len].copy_from_slice(&bytes[..len]);
            read.give_up(req);
            return Some(len);
        }
        self.close(req);
        None
    }

    /// `seek_cb`.
    pub(crate) fn seek(&mut self, req: &mut Request, offset: i64, origin: i32) -> bool {
        match self.stream.as_deref() {
            Some(stream) => stream.seek(req, offset, Whence::from_int(i64::from(origin))),
            None => false,
        }
    }

    /// `free_cb`: the stream closed.
    pub(crate) fn close(&mut self, req: &mut Request) {
        if let Some(stream) = self.stream.take() {
            stream.free(req);
        }
    }
}

/// `class CURLFile`, `@not-serializable`.
///
/// ```php
/// $f = new CURLFile('/tmp/x', 'text/plain');
/// var_dump($f, $f->getFilename(), $f->getMimeType(), $f->getPostFilename());
/// $f->setPostFilename('y.txt');
/// $f->setMimeType('a/b');
/// var_dump($f->postname, $f->mime);
/// try { serialize($f); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { new CURLFile("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(CURLFile)#1 (3) {
///   ["name"]=>
///   string(6) "/tmp/x"
///   ["mime"]=>
///   string(10) "text/plain"
///   ["postname"]=>
///   string(0) ""
/// }
/// string(6) "/tmp/x"
/// string(10) "text/plain"
/// string(0) ""
/// string(5) "y.txt"
/// string(3) "a/b"
/// Serialization of 'CURLFile' is not allowed
/// CURLFile::__construct(): Argument #1 ($filename) must not contain any null bytes
/// ```
#[php_class(not_serializable)]
pub(crate) struct CURLFile;

#[php_class]
impl CURLFile {
    #[php(property)]
    const name: PhpString = "";
    #[php(property)]
    const mime: PhpString = "";
    #[php(property)]
    const postname: PhpString = "";

    fn __construct(
        req: &mut Request,
        this: This<Self>,
        filename: Path,
        #[php(default = null)] mime_type: Option<PhpString>,
        #[php(default = null)] posted_filename: Option<PhpString>,
    ) -> Result<(), Thrown> {
        construct(req, this.object(), filename.0, mime_type, posted_filename)
    }

    #[php(tentative)]
    fn getFilename(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        property(req, this.object(), b"name")
    }

    #[php(tentative)]
    fn getMimeType(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        property(req, this.object(), b"mime")
    }

    #[php(tentative)]
    fn getPostFilename(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        property(req, this.object(), b"postname")
    }

    #[php(tentative)]
    fn setMimeType(
        req: &mut Request,
        this: This<Self>,
        mime_type: PhpString,
    ) -> Result<(), Thrown> {
        let mime_type = Owner::take(req, Value::string(mime_type));
        write_prop(req, &this.object(), b"mime", mime_type)
    }

    #[php(tentative)]
    fn setPostFilename(
        req: &mut Request,
        this: This<Self>,
        posted_filename: PhpString,
    ) -> Result<(), Thrown> {
        let posted_filename = Owner::take(req, Value::string(posted_filename));
        write_prop(req, &this.object(), b"postname", posted_filename)
    }
}

/// `curlfile_get_property`: the property as a string.
fn property(req: &mut Request, object: PhpObject, name: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    let value = read_prop(req, &object, name)?.deref();
    let string = to_string(req, &value)?;
    Ok(string)
}

/// `curlfile_ctor`.
fn construct(
    req: &mut Request,
    object: PhpObject,
    filename: PhpString,
    mime: Option<PhpString>,
    postname: Option<PhpString>,
) -> Result<(), Thrown> {
    let fields = [
        (b"name".as_slice(), Some(filename)),
        (b"mime", mime),
        (b"postname", postname),
    ];
    for (name, value) in fields {
        if let Some(value) = value {
            let value = Owner::take(req, Value::string(value));
            write_prop(req, &object, name, value)?;
        }
    }
    Ok(())
}

/// `curl_file_create(string $filename, ?string $mime_type = null, ?string $posted_filename = null): CURLFile`.
///
/// ```php
/// var_dump(curl_file_create('/tmp/x', null, 'up.txt'));
/// ```
/// ```expect
/// object(CURLFile)#1 (3) {
///   ["name"]=>
///   string(6) "/tmp/x"
///   ["mime"]=>
///   string(0) ""
///   ["postname"]=>
///   string(6) "up.txt"
/// }
/// ```
#[php_function]
fn curl_file_create(
    req: &mut Request,
    filename: Path,
    #[php(default = null)] mime_type: Option<PhpString>,
    #[php(default = null)] posted_filename: Option<PhpString>,
) -> Result<Owner<Instance<CURLFile>>, Thrown> {
    let object = made(req, b"CURLFile")?;
    if let Err(thrown) = construct(req, *object, filename.0, mime_type, posted_filename) {
        object.give_up(req);
        return Err(thrown);
    }
    Ok(object.into_instance())
}

/// `class CURLStringFile`: a file part whose bytes are a string.
///
/// ```php
/// var_dump(new CURLStringFile('data', 'a.txt'), new CURLStringFile('d', 'b', 'text/csv'));
/// ```
/// ```expect
/// object(CURLStringFile)#1 (3) {
///   ["data"]=>
///   string(4) "data"
///   ["postname"]=>
///   string(5) "a.txt"
///   ["mime"]=>
///   string(24) "application/octet-stream"
/// }
/// object(CURLStringFile)#2 (3) {
///   ["data"]=>
///   string(1) "d"
///   ["postname"]=>
///   string(1) "b"
///   ["mime"]=>
///   string(8) "text/csv"
/// }
/// ```
#[php_class]
pub(crate) struct CURLStringFile;

#[php_class]
impl CURLStringFile {
    #[php(property)]
    const data: PhpString;
    #[php(property)]
    const postname: PhpString;
    #[php(property)]
    const mime: PhpString;

    fn __construct(
        req: &mut Request,
        this: This<Self>,
        data: PhpString,
        postname: PhpString,
        #[php(default = "application/octet-stream")] mime: PhpString,
    ) -> Result<(), Thrown> {
        let object = this.object();
        for (name, value) in [
            (b"data".as_slice(), data),
            (b"postname", postname),
            (b"mime", mime),
        ] {
            let value = Owner::take(req, Value::string(value));
            write_prop(req, &object, name, value)?;
        }
        Ok(())
    }
}

/// Whether `value` is an object of `class` or a class extending it.
fn is_a(req: &mut Request, value: &Value, class: &[u8]) -> Result<Option<PhpObject>, Thrown> {
    let Some(object) = value.as_object().copied() else {
        return Ok(None);
    };
    let Some(id) = lookup_class(req, class)? else {
        return Ok(None);
    };
    Ok(object.instance_of(req, id).then_some(object))
}

/// A string property's bytes, `None` for one that holds no string.
fn prop_bytes(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
) -> Result<Option<Vec<u8>>, Thrown> {
    let value = read_prop(req, &object, name)?.deref();
    Ok(value.as_str_bytes().map(<[u8]>::to_vec))
}

/// `ch->postfields` replaced by `array`, one more owner of it.
fn keep_postfields(req: &mut Request, object: PhpObject, array: PhpArray) {
    let value = Value::array(array);
    req.addref(&value);
    let old = object
        .native_mut::<Curl>(req)
        .and_then(|curl| curl.postfields.replace(value));
    if let Some(old) = old {
        req.release(old);
    }
}

/// One part per element of a list under `name`:
/// `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(current), …)`
/// (`Z/ext/curl/interface.c:1519`), held by the row, as php's is.
fn add_list(
    req: &mut Request,
    mime: &mut MimeBuilder,
    name: &[u8],
    list: PhpArray,
) -> Result<(), Thrown> {
    walk::foreach(req, list, Hold::None, |req, row| -> walk::Step {
        with_string(req, row.value(), |data| mime.add_data(name, data))?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// `build_mime_structure_from_hash`: one part per element, a
/// `CURLFile` read from its path, a `CURLStringFile` from its data, an
/// array one part per element under its key, anything else its string;
/// then `CURLOPT_MIMEPOST`.
pub(crate) fn build_mime(
    req: &mut Request,
    object: PhpObject,
    array: PhpArray,
) -> Result<bool, Thrown> {
    let easy = easy_of(req, object)?;
    let Some(mut mime) = easy.mime() else {
        return Ok(false);
    };
    let mut error = 0;
    // `ZEND_HASH_FOREACH_KEY_VAL(postfields, …)` (`Z/ext/curl/interface.c:1387`)
    // holds nothing: the option's argument holds the table.
    let walked = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step<()> {
        let key = row.key().clone();
        let value = row.value().deref();
        let name = match key {
            Key::Int(n) => n.to_string().into_bytes(),
            Key::Str(s) => s.as_bytes().to_vec(),
        };
        let value = value.deref();
        if let Some(file) = is_a(req, &value, b"CURLFile")? {
            let Some(path) = prop_bytes(req, file, b"name")? else {
                let text = format!(
                    "Invalid filename for key {}",
                    String::from_utf8_lossy(&name)
                );
                docref(req, Level::Warning, &text);
                return Ok(ControlFlow::Continue(()));
            };
            if req.check_open_basedir(&path, true).is_err() {
                return Ok(ControlFlow::Break(()));
            }
            let kind = prop_bytes(req, file, b"mime")?.filter(|m| !m.is_empty());
            let postname = prop_bytes(req, file, b"postname")?.filter(|p| !p.is_empty());
            keep_postfields(req, object, array);
            let opening = Opening {
                must_seek: true,
                ..Opening::default()
            };
            let stream = req.open_stream_with(&path, b"rb", opening).ok().flatten();
            let size = stream
                .as_deref()
                .and_then(|stream| stream.stat(req))
                .map(|stat| stat[7]);
            let source = Source {
                filename: path.clone(),
                stream,
            };
            let kind = kind.unwrap_or_else(|| b"application/octet-stream".to_vec());
            let filename = postname.unwrap_or(path);
            match mime.add_file(req, &name, source, size, &filename, &kind) {
                Some(0) => {}
                Some(code) => error = code,
                None => {
                    return Ok(ControlFlow::Break(()));
                }
            }
            return Ok(ControlFlow::Continue(()));
        }
        if let Some(file) = is_a(req, &value, b"CURLStringFile")? {
            let postname = prop_bytes(req, file, b"postname")?.unwrap_or_default();
            let kind = prop_bytes(req, file, b"mime")?.unwrap_or_default();
            let data = prop_bytes(req, file, b"data")?.unwrap_or_default();
            keep_postfields(req, object, array);
            match mime.add_string_file(&name, &data, &postname, &kind) {
                Some(0) => {}
                Some(code) => error = code,
                None => {
                    return Ok(ControlFlow::Break(()));
                }
            }
            return Ok(ControlFlow::Continue(()));
        }
        if let ValueRef::Array(inner) = value.as_ref() {
            add_list(req, &mut mime, &name, *inner)?;
            return Ok(ControlFlow::Continue(()));
        }
        with_string(req, &value, |data| mime.add_data(&name, data))?;
        Ok(ControlFlow::Continue(()))
    })?;
    // A part not added ended the walk, and the parts made are given back.
    if walked.is_some() {
        let thrown = mime.discard(req);
        return thrown.map_or(Ok(false), Err);
    }
    if let Some(curl) = object.native_mut::<Curl>(req) {
        curl.err = i64::from(error);
    }
    if error != 0 {
        let thrown = mime.discard(req);
        return thrown.map_or(Ok(false), Err);
    }
    let (code, thrown) = easy.set_mime(req, mime.build());
    if let Some(curl) = object.native_mut::<Curl>(req) {
        curl.err = i64::from(code);
    }
    match thrown {
        Some(thrown) => Err(thrown),
        None => Ok(code == 0),
    }
}
