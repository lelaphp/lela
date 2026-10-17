//! `curl_file.c`: the `CURLFile` and `CURLStringFile` classes and
//! `curl_file_create`; with `build_mime_structure_from_hash`, which turns
//! a `CURLOPT_POSTFIELDS` array into a MIME form, and the file a
//! `CURLFile` part reads while the transfer runs.

use std::ops::ControlFlow;

use php_ext::abi::{FromArgument as _, Instance, Path, Raw, This};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{lookup_class, read_prop, write_prop};
use php_ext::coerce::with_string;
use php_ext::raising::{Level, Thrown, docref, while_pending};
use php_ext::request::{Opening, OwnedStream as _, Request, Stream, Streams as _, Whence};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, PropertyScope, Value, ValueRef};

use crate::ffi::{MimeBuilder, Posted};
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
    fn getFilename(req: &mut Request, this: This<Self>) -> Result<Raw<PhpString>, Thrown> {
        property(req, this.object(), b"name")
    }

    #[php(tentative)]
    fn getMimeType(req: &mut Request, this: This<Self>) -> Result<Raw<PhpString>, Thrown> {
        property(req, this.object(), b"mime")
    }

    #[php(tentative)]
    fn getPostFilename(req: &mut Request, this: This<Self>) -> Result<Raw<PhpString>, Thrown> {
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

/// `curlfile_get_property`: the property as the silent read answers it,
/// `RETURN_COPY_DEREF`, under the method's declared `string`, so an
/// unset one is `null`, as php checks no internal return in a release
/// build.
///
/// ```php
/// $f = new CURLFile('/etc/hosts'); unset($f->name);
/// $g = new CURLFile('/etc/hosts'); unset($g->mime);
/// var_dump($f->getFilename(), $g->getMimeType(), $g->getPostFilename());
/// echo (new ReflectionMethod('CURLFile', 'getFilename'))->getTentativeReturnType(), "\n";
/// ```
/// ```expect
/// NULL
/// NULL
/// string(0) ""
/// string
/// ```
fn property(req: &mut Request, object: PhpObject, name: &[u8]) -> Result<Raw<PhpString>, Thrown> {
    let value = read_prop(req, &object, name)?.deref();
    Raw::from_value(req, value)
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

/// A string property's bytes, `None` for one that holds no string, read
/// as `zend_read_property(ce, obj, name, /* silent */ false, &rv)` reads
/// it in the declaring class's scope: an unset typed property is php's
/// `Error`, the `Err`. The form walk goes on past each throw, as php's
/// does, and the request's throwable is the first with each later one
/// its previous.
///
/// ```php
/// $f = new CURLFile('/etc/hosts'); unset($f->name);
/// $g = new CURLFile('/etc/hosts'); unset($g->mime); unset($g->postname);
/// $s = new CURLStringFile('d', 'p'); unset($s->data);
/// $c = curl_init('file:///nonexistent');
/// try { var_dump(curl_setopt($c, CURLOPT_POSTFIELDS, ['f' => $f, 'x' => 'y'])); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { var_dump(curl_setopt($c, CURLOPT_POSTFIELDS, ['g' => $g, 'x' => 'y'])); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; var_dump($e->getPrevious()?->getMessage()); }
/// try { var_dump(curl_setopt($c, CURLOPT_POSTFIELDS, ['s' => $s])); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Warning: curl_setopt(): Invalid filename for key f in %s on line 6
/// Error: Typed property CURLFile::$name must not be accessed before initialization
/// Error: Typed property CURLFile::$postname must not be accessed before initialization
/// string(73) "Typed property CURLFile::$mime must not be accessed before initialization"
/// Error: Typed property CURLStringFile::$data must not be accessed before initialization
/// ```
fn prop_bytes(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
) -> Result<Option<Vec<u8>>, Thrown> {
    let value = object.read_prop_in(req, PropertyScope::Declaring, name)?;
    let bytes = value.deref().as_str_bytes().map(<[u8]>::to_vec);
    value.give_up(req);
    Ok(bytes)
}

/// `ch->postfields` replaced by `array`, one more owner of it.
fn keep_postfields(req: &mut Request, object: PhpObject, array: PhpArray) {
    let held = Owner::take(req, array);
    let old = match object.native_mut::<Curl>(req) {
        Some(curl) => curl.postfields.replace(held),
        None => Some(held),
    };
    if let Some(old) = old {
        old.give_up(req);
    }
}

/// `build_mime_structure_from_hash`: one part per element, a
/// `CURLFile` read from its path, a `CURLStringFile` from its data, an
/// array one part per element under its key, anything else its string;
/// then `CURLOPT_MIMEPOST`. The tree is set or freed as the walk ends,
/// whichever way it ends ([`EasyId::post_form`]).
///
/// A throw while an element is read does not end the walk: php's
/// `zval_get_tmp_string` answers the empty string with the throwable
/// left in `EG(exception)`, and the walk goes on with it pending, so a
/// later `__toString` is not called, a later warning is still raised and
/// the form is still set; the throwable leaves after.
///
/// ```php
/// class T { function __toString(): string { throw new Exception("no"); } }
/// $f = tempnam(sys_get_temp_dir(), 'curl');
/// file_put_contents($f, "x");
/// $c = curl_init('file:///nonexistent');
/// try { curl_setopt($c, CURLOPT_POSTFIELDS, ['a' => new CURLFile($f), 'b' => new T]); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// unlink($f);
/// echo "end\n";
/// ```
/// ```expect capture
/// no
/// end
/// ```
///
/// ```php
/// class T { function __toString(): string { throw new Exception("no"); } }
/// class U { function __toString(): string { echo "U called\n"; return "u"; } }
/// ini_set('open_basedir', '/usr');
/// $c = curl_init('file:///nonexistent');
/// try { var_dump(curl_setopt($c, CURLOPT_POSTFIELDS, ['b' => new T, 'u' => new U, 'c' => new CURLFile('/etc/passwd')])); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { var_dump(curl_setopt($c, CURLOPT_POSTFIELDS, ['b' => new T, 'c' => [new U, 1], 'd' => new U])); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { var_dump(curl_setopt($c, CURLOPT_POSTFIELDS, ['b' => new T, 's' => new CURLStringFile('d', 'p')])); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// var_dump(curl_errno($c));
/// ```
/// ```expect capture
/// Warning: curl_setopt(): open_basedir restriction in effect. File(/etc/passwd) is not within the allowed path(s): (/usr) in %SCRATCH%/witness.php on line 6
/// no
/// no
/// no
/// int(0)
/// ```
///
/// [`EasyId::post_form`]: crate::ffi::EasyId::post_form
pub(crate) fn build_mime(
    req: &mut Request,
    object: PhpObject,
    array: PhpArray,
) -> Result<bool, Thrown> {
    let easy = easy_of(req, object)?;
    let mut pending = None;
    let posted = easy.post_form(req, |req, mime| {
        let mut form = Form {
            mime,
            object,
            array,
            error: 0,
            pending: None,
        };
        let walked = form.fill(req);
        // `SAVE_CURL_ERROR(ch, error)` where the walk reached its end.
        if walked && let Some(curl) = object.native_mut::<Curl>(req) {
            curl.err = i64::from(form.error);
        }
        pending = form.pending;
        walked && form.error == 0
    });
    let Some((posted, thrown)) = posted else {
        return Ok(false);
    };
    if let Posted::Set(code) = posted
        && let Some(curl) = object.native_mut::<Curl>(req)
    {
        curl.err = i64::from(code);
    }
    let thrown = match thrown {
        Some(later) => Some(raised(req, pending, later)),
        None => pending,
    };
    match thrown {
        Some(thrown) => Err(thrown),
        None => Ok(posted == Posted::Set(0)),
    }
}

/// `later` raised while `earlier` stands pending: chained onto it, as
/// `zend_throw_exception_internal` chains one raised while another is
/// pending. A call made while a throwable stands answers that same
/// throwable, having served nothing, which is no second throwable.
fn raised(req: &mut Request, earlier: Option<Thrown>, later: Thrown) -> Thrown {
    match earlier {
        Some(earlier) if earlier.exception() == later.exception() => earlier,
        Some(earlier) => earlier.chain(req, later),
        None => later,
    }
}

/// `build_mime_structure_from_hash`'s walk over `array`, the option's
/// argument, for the `CurlHandle` `object`: the tree being built, php's
/// `error`, and what the walk left in `EG(exception)`, which php's walk
/// goes on past.
struct Form<'m> {
    mime: &'m mut MimeBuilder,
    object: PhpObject,
    array: PhpArray,
    error: i32,
    pending: Option<Thrown>,
}

impl Form<'_> {
    /// The walk: whether it reached its end, where an element may end it
    /// early (`goto out_string`).
    fn fill(&mut self, req: &mut Request) -> bool {
        // `ZEND_HASH_FOREACH_KEY_VAL(postfields, …)` (`Z/ext/curl/interface.c:1387`)
        // holds nothing: the option's argument holds the table.
        let walked = walk::foreach(req, self.array, Hold::None, |req, row| -> walk::Step<()> {
            let name = match row.key() {
                Key::Int(n) => n.to_string().into_bytes(),
                Key::Str(s) => s.as_bytes().to_vec(),
            };
            let value = row.value().deref().deref();
            Ok(self.element(req, &name, &value))
        });
        match walked {
            Ok(ended) => ended.is_none(),
            Err(thrown) => {
                self.park(req, thrown);
                false
            }
        }
    }

    /// One element of the form, its parts added under `name`.
    fn element(&mut self, req: &mut Request, name: &[u8], value: &Value) -> ControlFlow<()> {
        match self.step(req, |req, _| is_a(req, value, b"CURLFile")) {
            Some(Some(file)) => return self.file(req, name, file),
            Some(None) => {}
            None => return self.flow(),
        }
        match self.step(req, |req, _| is_a(req, value, b"CURLStringFile")) {
            Some(Some(file)) => return self.string_file(req, name, file),
            Some(None) => {}
            None => return self.flow(),
        }
        if let ValueRef::Array(inner) = value.as_ref() {
            return self.list(req, name, *inner);
        }
        self.field(req, name, value)
    }

    /// A `CURLFile` part, read from its path by the transfer: a stream
    /// opened on it now, held by the part until its `free_cb`. Each
    /// property read that throws leaves the throwable pending and reads
    /// as no string, as php's `zend_read_property` answers then.
    fn file(&mut self, req: &mut Request, name: &[u8], file: PhpObject) -> ControlFlow<()> {
        let Some(path) = self.prop(req, file, b"name") else {
            let text = format!("Invalid filename for key {}", String::from_utf8_lossy(name));
            self.step(req, |req, _| {
                docref(req, Level::Warning, &text);
                Ok(())
            });
            return self.flow();
        };
        match self.step(req, |req, _| {
            Ok(req.check_open_basedir(&path, true).is_ok())
        }) {
            Some(true) => {}
            // `goto out_string`.
            Some(false) => return ControlFlow::Break(()),
            None => return self.flow(),
        }
        let kind = self.prop(req, file, b"mime").filter(|m| !m.is_empty());
        let postname = self.prop(req, file, b"postname").filter(|p| !p.is_empty());
        let (object, array) = (self.object, self.array);
        let added = self.step(req, |req, mime| {
            keep_postfields(req, object, array);
            let opening = Opening {
                must_seek: true,
                ..Opening::default()
            };
            // A wrapper's throw leaves the part with no stream, as php's
            // `php_stream_open_wrapper` answers `NULL` with it pending.
            let (stream, thrown) = match req.open_stream_with(&path, b"rb", opening) {
                Ok(stream) => (stream, None),
                Err(thrown) => (None, Some(thrown)),
            };
            let size = stream
                .as_deref()
                .and_then(|stream| stream.stat(req))
                .map(|stat| stat[7]);
            let source = Source {
                filename: path.clone(),
                stream,
            };
            let kind = kind.unwrap_or_else(|| b"application/octet-stream".to_vec());
            let filename = postname.unwrap_or_else(|| path.clone());
            let code = mime.add_file(req, name, source, size, &filename, &kind);
            Ok((code, thrown))
        });
        let Some((code, thrown)) = added else {
            return self.flow();
        };
        if let Some(thrown) = thrown {
            self.park(req, thrown);
        }
        match code {
            Some(0) => {}
            Some(code) => self.error = code,
            None => return ControlFlow::Break(()),
        }
        self.flow()
    }

    /// A `CURLStringFile` part: its data, file name and type.
    fn string_file(&mut self, req: &mut Request, name: &[u8], file: PhpObject) -> ControlFlow<()> {
        // `if (EG(exception)) goto out_string;` after each read, which
        // finds a throwable the walk left pending as well.
        if self.pending.is_some() {
            return ControlFlow::Break(());
        }
        let (object, array) = (self.object, self.array);
        let added = self.step(req, |req, mime| {
            let postname = prop_bytes(req, file, b"postname")?.unwrap_or_default();
            let kind = prop_bytes(req, file, b"mime")?.unwrap_or_default();
            let data = prop_bytes(req, file, b"data")?.unwrap_or_default();
            keep_postfields(req, object, array);
            Ok(mime.add_string_file(name, &data, &postname, &kind))
        });
        match added.flatten() {
            Some(0) => {}
            Some(code) => self.error = code,
            None => return ControlFlow::Break(()),
        }
        ControlFlow::Continue(())
    }

    /// One part per element of a list under `name`:
    /// `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(current), …)`
    /// (`Z/ext/curl/interface.c:1519`), held by the row, as php's is.
    fn list(&mut self, req: &mut Request, name: &[u8], list: PhpArray) -> ControlFlow<()> {
        let walked = walk::foreach(req, list, Hold::None, |req, row| -> walk::Step {
            Ok(self.field(req, name, row.value()))
        });
        if let Err(thrown) = walked {
            self.park(req, thrown);
        }
        self.flow()
    }

    /// `add_simple_field`: a part named `name` holding `value`'s text,
    /// the empty string where its conversion throws, as
    /// `zval_get_tmp_string` answers then; the code is not read, as
    /// php's callers do not read it.
    fn field(&mut self, req: &mut Request, name: &[u8], value: &Value) -> ControlFlow<()> {
        let added = self.step(req, |req, mime| {
            with_string(req, value, |data| mime.add_data(name, data))
        });
        if added.is_none() {
            self.mime.add_data(name, b"");
        }
        self.flow()
    }

    /// `step` taken as php takes it, with what the walk left pending
    /// standing in `EG(exception)` ([`while_pending`]), so a call it
    /// makes returns at once and a diagnostic it raises reaches no
    /// handler; what it throws is left pending in turn, and `None`.
    /// `None` too where what stands is a fatal or an `exit`, which
    /// [`Form::flow`] ends the walk on before any further step.
    fn step<R>(
        &mut self,
        req: &mut Request,
        step: impl FnOnce(&mut Request, &mut MimeBuilder) -> Result<R, Thrown>,
    ) -> Option<R> {
        let mime = &mut *self.mime;
        let taken = match &self.pending {
            None => Some(step(req, mime)),
            Some(pending) => while_pending(req, pending, |req| step(req, mime)),
        };
        match taken? {
            Ok(answer) => Some(answer),
            Err(thrown) => {
                self.park(req, thrown);
                None
            }
        }
    }

    /// A string property of `file`, read as a step: `None` for one that
    /// holds no string or whose read threw, the throwable left pending.
    fn prop(&mut self, req: &mut Request, file: PhpObject, name: &[u8]) -> Option<Vec<u8>> {
        self.step(req, |req, _| prop_bytes(req, file, name))
            .flatten()
    }

    /// `thrown` left pending ([`raised`]).
    fn park(&mut self, req: &mut Request, thrown: Thrown) {
        self.pending = Some(raised(req, self.pending.take(), thrown));
    }

    /// Whether the walk goes on: past a throwable, as php's loop goes on
    /// with `EG(exception)` set; not past a fatal or an `exit`, which
    /// leave by `zend_bailout` at once.
    fn flow(&self) -> ControlFlow<()> {
        match &self.pending {
            Some(thrown) if thrown.exception().is_none() => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        }
    }
}
