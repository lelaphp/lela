//! `curl_file.c`: the `CURLFile` and `CURLStringFile` classes and
//! `curl_file_create`; with `build_mime_structure_from_hash`, which turns
//! a `CURLOPT_POSTFIELDS` array into a MIME form, and the file a
//! `CURLFile` part reads while the transfer runs.

use std::ops::ControlFlow;

use php_ext::abi::{FromArgument as _, Instance, Path, Raw, This};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{lookup_class, read_prop, write_prop};
use php_ext::coerce::with_string;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Opening, OwnedStream as _, Request, Stream, Streams as _, Whence};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, PropertyScope, Value, ValueRef};

use crate::ffi::{MimeBuilder, Posted};
use crate::handle::{Curl, easy_of, made};
use crate::throwables::Pending;

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
    let Some(object) = value.as_object() else {
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
/// An `exit` while an element is read stands as php's unwind exit does,
/// and the walk goes on past it as past a throwable, so the form is still
/// set:
///
/// ```php
/// class X { function __toString(): string { exit("exit\n"); } }
/// $h = curl_init('http://127.0.0.1:1/');
/// register_shutdown_function(function () use ($h) { curl_exec($h); var_dump(curl_getinfo($h, CURLINFO_EFFECTIVE_METHOD)); });
/// curl_setopt($h, CURLOPT_POSTFIELDS, ['a' => new X, 'b' => 'later']);
/// echo "unreached\n";
/// ```
/// ```expect
/// exit
/// string(4) "POST"
/// ```
///
/// The trees the `CURL *` held are let go of as the walk ends, before
/// the new one is set, as php cleans `ch->to_free->post` before
/// `curl_easy_setopt(CURLOPT_MIMEPOST)`; a walk that ends early
/// (`goto out_string`) lets go of the new tree alone. Either free is made
/// under what the walk left standing, so a file part's `free_cb`
/// closing a stream through a wrapper calls nothing, as php's
/// `zend_call_function` returns at once under `EG(exception)`.
///
/// ```php
/// class Mem {
///     public $context;
///     private $at = 0;
///     function stream_open($path, $mode, $options, &$opened) { echo "open $path\n"; return true; }
///     function stream_read($n) { $r = substr("hello", $this->at, $n); $this->at += strlen($r); return $r; }
///     function stream_eof() { return $this->at >= 5; }
///     function stream_seek($to, $whence) { $this->at = $to; return true; }
///     function stream_tell() { return $this->at; }
///     function stream_stat() { return ['size' => 5]; }
///     function stream_close() { echo "close\n"; }
/// }
/// stream_wrapper_register('mem', 'Mem');
/// class S extends CURLStringFile {
///     function __construct() { parent::__construct('d', 'p'); unset($this->postname); }
///     function __get($n) { throw new Exception("read $n"); }
/// }
/// class T { function __toString(): string { throw new Exception("string"); } }
/// $srv = stream_socket_server('tcp://127.0.0.1:0');
/// $h = curl_init('http://' . stream_socket_get_name($srv, false) . '/');
/// curl_setopt_array($h, [CURLOPT_RETURNTRANSFER => true, CURLOPT_HTTPHEADER => ['Expect:']]);
/// curl_setopt($h, CURLOPT_POSTFIELDS, ['f' => new CURLFile('mem://x')]);
/// $m = curl_multi_init();
/// curl_multi_add_handle($m, $h);
/// $conn = null; $got = '';
/// do {
///     curl_multi_exec($m, $running);
///     $r = [$conn ?? $srv]; $w = $e = null;
///     if ($conn !== false && stream_select($r, $w, $e, 0, 10000)) {
///         if ($conn === null) { $conn = stream_socket_accept($srv); continue; }
///         $got .= fread($conn, 8192);
///         if (str_ends_with($got, "--\r\n")) {
///             fwrite($conn, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
///             fclose($conn);
///             $conn = false;
///         }
///     }
///     curl_multi_select($m, 0.01);
/// } while ($running);
/// curl_multi_remove_handle($m, $h);
/// echo "posted hello: ", var_export(str_contains($got, "\r\n\r\nhello\r\n"), true), ", answered ", curl_multi_getcontent($h), "\n";
/// foreach ([['g' => new CURLFile('mem://y'), 's' => new S], ['t' => new T], ['z' => new CURLFile('mem://z')], ['a' => 'b']] as $form) {
///     echo "set ", implode(',', array_keys($form)), "\n";
///     try { var_dump(curl_setopt($h, CURLOPT_POSTFIELDS, $form)); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// }
/// echo "end\n";
/// ```
/// ```expect
/// open mem://x
/// posted hello: true, answered ok
/// set g,s
/// open mem://y
/// read postname
/// set t
/// string
/// set z
/// open mem://z
/// bool(true)
/// set a
/// close
/// bool(true)
/// end
/// ```
///
/// What the walk leaves pending, and what the trees' frees or the set
/// threw, is left in `pending`, which may stand already as the walk
/// starts, an earlier option's in `curl_setopt_array`.
///
/// [`EasyId::post_form`]: crate::ffi::EasyId::post_form
pub(crate) fn build_mime(
    req: &mut Request,
    object: PhpObject,
    array: PhpArray,
    pending: &mut Pending,
) -> Result<bool, Thrown> {
    let easy = easy_of(req, object)?;
    let posted = easy.post_form(req, pending, |req, mime, pending| {
        let mut form = Form {
            mime,
            object,
            array,
            error: 0,
            pending,
        };
        let walked = form.fill(req);
        // `SAVE_CURL_ERROR(ch, error)` where the walk reached its end.
        if walked && let Some(curl) = object.native_mut::<Curl>(req) {
            curl.err = i64::from(form.error);
        }
        walked && form.error == 0
    });
    let Some(posted) = posted else {
        return Ok(false);
    };
    if let Posted::Set(code) = posted
        && let Some(curl) = object.native_mut::<Curl>(req)
    {
        curl.err = i64::from(code);
    }
    Ok(posted == Posted::Set(0))
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
    pending: &'m mut Pending,
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
            None => return self.flow(req),
        }
        match self.step(req, |req, _| is_a(req, value, b"CURLStringFile")) {
            Some(Some(file)) => return self.string_file(req, name, file),
            Some(None) => {}
            None => return self.flow(req),
        }
        if let ValueRef::Array(inner) = value.as_ref() {
            return self.list(req, name, inner);
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
            return self.flow(req);
        };
        match self.step(req, |req, _| {
            Ok(req
                .environment_mut()
                .check_open_basedir(&path, true)
                .is_ok())
        }) {
            Some(true) => {}
            // `goto out_string`.
            Some(false) => return ControlFlow::Break(()),
            None => return self.flow(req),
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
            return self.flow(req);
        };
        if let Some(thrown) = thrown {
            self.park(req, thrown);
        }
        match code {
            Some(0) => {}
            Some(code) => self.error = code,
            None => return ControlFlow::Break(()),
        }
        self.flow(req)
    }

    /// A `CURLStringFile` part: its data, file name and type.
    fn string_file(&mut self, req: &mut Request, name: &[u8], file: PhpObject) -> ControlFlow<()> {
        // `if (EG(exception)) goto out_string;` after each read, which
        // finds a throwable the walk left pending as well.
        if self.pending.stands(req) {
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
        self.flow(req)
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
        self.flow(req)
    }

    /// `step` taken as php takes it, with what the walk left pending
    /// standing in `EG(exception)` ([`Pending::step`]); what it throws is
    /// left pending in turn, and `None`. `None` too where what stands is
    /// a fatal, which [`Form::flow`] ends the walk on before any further
    /// step.
    fn step<R>(
        &mut self,
        req: &mut Request,
        step: impl FnOnce(&mut Request, &mut MimeBuilder) -> Result<R, Thrown>,
    ) -> Option<R> {
        let mime = &mut *self.mime;
        match self.pending.step(req, |req, _| step(req, mime))? {
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

    /// `thrown` left pending ([`Pending::park`]).
    fn park(&mut self, req: &mut Request, thrown: Thrown) {
        self.pending.park(req, thrown);
    }

    /// Whether the walk goes on: past a throwable or an `exit`, as php's
    /// loop goes on with either in `EG(exception)`; not past a fatal,
    /// which leaves by `zend_bailout` at once.
    fn flow(&self, req: &Request) -> ControlFlow<()> {
        if self.pending.fatal(req) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}
