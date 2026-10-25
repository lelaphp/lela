//! What `php_libxml.c`'s loader decides once libxml has called it
//! ([`crate::ffi`] holds libxml's side of each callback): a path opened
//! through php's wrappers as `php_libxml_streams_IO_open_wrapper` opens it,
//! and a program's resolver called as `php_libxml_external_entity_loader`
//! calls it. Everything here runs on the request the running call lent,
//! through the engine's one wrapper table and streams layer.

use php_ext::abi::Callable;
use php_ext::call::{call, closure_from_callable};
use php_ext::raising::{Level, Thrown, docref, type_error};
use php_ext::request::{Request, Stream, StreamCtx, Streams as _, UrlStat};
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef};

use crate::State;
use crate::ffi::{Context, Uri};

/// What a program's resolver answered, as `php_libxml_external_entity_loader`
/// reads its return value.
pub(crate) enum Resolved {
    /// The call failed, `Z_ISUNDEF(retval)`: the resolver threw, and the
    /// function php names in its error.
    Failed { name: String, thrown: Thrown },
    /// A throwable raised reading the answer: a resource that is no
    /// stream, or a value with no string form.
    Thrown(Thrown),
    /// A stream to read the entity from, its owner the input's.
    Stream(Owner<Stream>),
    /// A path to open as the default loader opens one.
    Path(Vec<u8>),
    /// `null`: no entity.
    Nothing,
}

/// `php_libxml.c`'s loader, over the request the running call lent:
/// `LIBXML(entity_loader_callback)` read and called, and paths opened,
/// read, written and closed through php's streams.
pub(crate) struct Loader;

impl Loader {
    /// Whether `libxml_set_external_entity_loader` set a resolver,
    /// `ZEND_FCC_INITIALIZED(LIBXML(entity_loader_callback))`.
    pub(crate) fn has_resolver(req: &mut Request) -> bool {
        req.state::<State>().entity_loader.is_some()
    }

    /// The resolver called with the public identifier, the system identifier
    /// and the parser's four members, and its answer read: a stream is the
    /// entity's text, and `null` is php's error, onto the list with internal
    /// errors on.
    ///
    /// ```php
    /// $xml = '<!DOCTYPE r [<!ENTITY e SYSTEM "e.xml"><!ENTITY p PUBLIC "-//P" "p.xml">]><r>&e;&p;</r>';
    /// $d = new DOMDocument;
    /// $d->substituteEntities = true;
    /// libxml_set_external_entity_loader(function ($public, $system, $context) {
    ///     echo "load ", var_export($public, true), " ", basename($system), " ", var_export($context['intSubName'], true), "\n";
    ///     if ($public === null) {
    ///         $f = fopen('php://memory', 'w+');
    ///         fwrite($f, 'from a stream');
    ///         rewind($f);
    ///         return $f;
    ///     }
    ///     return null;
    /// });
    /// $d->loadXML($xml);
    /// echo $d->documentElement->textContent, "\n";
    /// libxml_set_external_entity_loader(fn($p, $s, $c) => null);
    /// $d->loadXML('<!DOCTYPE r [<!ENTITY e SYSTEM "e.xml">]><r>&e;</r>');
    /// libxml_use_internal_errors(true);
    /// $d->loadXML('<!DOCTYPE r [<!ENTITY e SYSTEM "e.xml">]><r>&e;</r>');
    /// $e = libxml_get_errors()[0];
    /// var_dump($e->message, $e->line, $e->file);
    /// ```
    /// ```expect
    /// load NULL e.xml NULL
    /// load '-//P' p.xml NULL
    ///
    /// Warning: DOMDocument::loadXML(): Failed to load external entity "-//P" in %s on line 15
    /// from a stream
    ///
    /// Warning: DOMDocument::loadXML(): Failed to load external entity because the resolver function returned null in %s on line 18
    /// string(74) "Failed to load external entity because the resolver function returned null"
    /// int(0)
    /// string(0) ""
    /// ```
    pub(crate) fn resolve(
        req: &mut Request,
        id: Option<&[u8]>,
        url: Option<&[u8]>,
        members: &Context,
    ) -> Resolved {
        let loader = req.state::<State>().entity_loader.as_ref();
        let Some((callable, value)) = loader.map(|loader| ((**loader).clone(), loader.value()))
        else {
            return Resolved::Nothing;
        };
        let arguments = match arguments(req, id, url, members) {
            Ok(arguments) => arguments,
            Err(thrown) => return Resolved::Thrown(thrown),
        };
        let views = arguments.each_ref().map(|argument| **argument);
        let answered = call(req, &callable, &views);
        for argument in arguments {
            argument.give_up(req);
        }
        let retval = match answered {
            Ok(retval) => retval,
            Err(thrown) => {
                return Resolved::Failed {
                    name: function_name(req, &callable, &value, false),
                    thrown,
                };
            }
        };
        let resolved = read_answer(req, &retval, &callable, &value);
        retval.give_up(req);
        resolved
    }

    /// `php_libxml_input_buffer_create_filename`'s open: nothing while the
    /// loader is disabled, else the path opened for reading.
    pub(crate) fn open_input(
        req: &mut Request,
        path: &[u8],
    ) -> Result<Option<Owner<Stream>>, Thrown> {
        if req.state::<State>().entity_loader_disabled {
            return Ok(None);
        }
        open(req, path, b"rb", true)
    }

    /// `php_libxml_output_buffer_create_filename`'s open: a URI with a scheme
    /// tried unescaped first, and the path as written after, since it may be
    /// a file named with a `%`.
    pub(crate) fn open_output(
        req: &mut Request,
        path: &[u8],
    ) -> Result<Option<Owner<Stream>>, Thrown> {
        if contains_nul_escape(path) {
            docref(
                req,
                Level::Warning,
                "URI must not contain percent-encoded NUL bytes",
            );
            return Ok(None);
        }
        if Uri::parse(path).is_some_and(|uri| uri.scheme.is_some())
            && let Some(unescaped) = Uri::unescape(path)
            && let Some(stream) = open(req, &unescaped, b"wb", false)?
        {
            return Ok(Some(stream));
        }
        open(req, path, b"wb", false)
    }

    /// `php_stream_read`: up to `len` bytes, empty at the end, `None` for the
    /// read that failed.
    pub(crate) fn read(
        req: &mut Request,
        stream: Stream,
        len: usize,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let Some(chunk) = stream.read(req, len)? else {
            return Ok(None);
        };
        let bytes = chunk.as_bytes().to_vec();
        chunk.give_up(req);
        Ok(Some(bytes))
    }
}

/// `ZVAL_STRING` or `ZVAL_NULL` of the two identifiers, and the `$context`
/// array with its four keys in php's order.
fn arguments(
    req: &mut Request,
    id: Option<&[u8]>,
    url: Option<&[u8]>,
    members: &Context,
) -> Result<[Owner<Value>; 3], Thrown> {
    let string_or_null =
        |req: &mut Request, bytes: Option<&[u8]>| -> Result<Owner<Value>, Thrown> {
            match bytes {
                Some(b) => Ok(PhpString::new(req, b)?.into_value()),
                None => Ok(Owner::null()),
            }
        };
    let public = string_or_null(req, id)?;
    let system = string_or_null(req, url)?;
    let mut context = PhpArray::new(req)?;
    let rows: [(&[u8], &Option<Vec<u8>>); 4] = [
        (b"directory", &members.directory),
        (b"intSubName", &members.int_sub_name),
        (b"extSubURI", &members.ext_sub_uri),
        (b"extSubSystem", &members.ext_sub_system),
    ];
    for (key, member) in rows {
        let value = string_or_null(req, member.as_deref())?;
        context.set_str(req, key, value)?;
    }
    Ok([public, system, context.into_value()])
}

/// A string is a path, a stream resource the entity's stream, another
/// resource php's `TypeError`, `null` nothing, and anything else its
/// string form (`try_convert_to_string`).
fn read_answer(req: &mut Request, retval: &Value, callable: &Callable, loader: &Value) -> Resolved {
    let retval = retval.deref();
    match retval.as_ref() {
        ValueRef::Str(s) => Resolved::Path(s.as_bytes().to_vec()),
        ValueRef::Null => Resolved::Nothing,
        _ if retval.as_resource().is_some() => {
            if let Some(stream) = Stream::from_value(req, &retval) {
                // `GC_ADDREF(stream->res)`: the input holds the stream past
                // the return value.
                return Resolved::Stream(Owner::take(req, stream));
            }
            let name = function_name(req, callable, loader, true);
            let text = format!(
                "{}(): The user entity loader callback \"{name}\" has returned a resource, but it is not a stream",
                req.frames().active_function_name()
            );
            Resolved::Thrown(type_error(req, &text))
        }
        _ => match php_ext::coerce::with_string(req, &retval, <[u8]>::to_vec) {
            Ok(path) => Resolved::Path(path),
            Err(thrown) => Resolved::Thrown(thrown),
        },
    }
}

/// The function the resolver resolved to, as php names it:
/// `function_handler->common.function_name` alone, and with `member` the
/// callable's name, `zend_get_callable_name`, which for anything but a
/// closure object is the fake closure's scope and function.
fn function_name(req: &mut Request, callable: &Callable, loader: &Value, member: bool) -> String {
    let loader = loader.deref();
    if member && let ValueRef::Str(s) = loader.as_ref() {
        return String::from_utf8_lossy(s.as_bytes()).into_owned();
    }
    let is_closure = loader
        .as_object()
        .is_some_and(|object| object.closure(req).is_some());
    let Ok(closure) = closure_from_callable(req, callable) else {
        return String::new();
    };
    let Some((function, scope)) = closure.closure(req).map(|info| (info.function, info.scope))
    else {
        closure.give_up(req);
        return String::new();
    };
    let mut name = Vec::new();
    if member
        && !is_closure
        && let Some(class) = scope.and_then(|c| req.symbols().class_decl(c))
    {
        name.extend_from_slice(&class.name.0);
        name.extend_from_slice(b"::");
    }
    if let Some(decl) = req.symbols().function_decl(function) {
        name.extend_from_slice(&decl.name.0);
    }
    closure.give_up(req);
    String::from_utf8_lossy(&name).into_owned()
}

fn contains_nul_escape(path: &[u8]) -> bool {
    path.windows(3).any(|w| w == b"%00")
}

/// `php_libxml_streams_IO_open_wrapper`: a path with no scheme or a
/// `file` one unescaped; for reading, a wrapper that stats asked quietly
/// first and a path it cannot stat left unopened and unwarned, since
/// libxml tries paths that need not exist (a DTD); then the stream opened
/// under `REPORT_ERRORS`. A document is read, and saved, through a
/// program's own wrapper as through the plain files one, opened with
/// what `libxml_set_streams_context` kept (`LIBXML(stream_context)`), and
/// with `FG(default_context)` where it kept none
/// (`php_stream_context_from_zval(NULL, 0)`).
///
/// ```php
/// class W { public $context;
///   function stream_open($path, $mode, $options, &$opened) { echo stream_context_get_options($this->context)['wr']['k'], "\n"; return false; }
///   function url_stat($path, $flags) { return []; }
/// }
/// stream_wrapper_register('wr', 'W');
/// stream_context_set_default(['wr' => ['k' => 'default']]);
/// @(new DOMDocument)->load('wr://x');
/// libxml_set_streams_context(stream_context_create(['wr' => ['k' => 'kept']]));
/// @(new DOMDocument)->load('wr://x');
/// @(new DOMDocument)->save('wr://y');
/// ```
/// ```expect
/// default
/// kept
/// kept
/// kept
/// ```
///
/// ```php
/// class Mem { public $context; static $data = ['doc' => '<r><a/></r>'];
///   private $at = 0; private $name;
///   function stream_open($path, $mode, $options, &$opened) { $this->name = substr($path, 6); return isset(self::$data[$this->name]) || $mode[0] == 'w'; }
///   function stream_read($n) { $s = substr(self::$data[$this->name], $this->at, $n); $this->at += strlen($s); return $s; }
///   function stream_write($s) { self::$data[$this->name] = (self::$data[$this->name] ?? '') . $s; return strlen($s); }
///   function stream_eof() { return $this->at >= strlen(self::$data[$this->name]); }
///   function url_stat($path, $flags) { return isset(self::$data[substr($path, 6)]) ? ['size' => 1] : false; }
/// }
/// stream_wrapper_register('mem', 'Mem');
/// $d = new DOMDocument;
/// var_dump($d->load('mem://doc'), $d->documentElement->firstChild->nodeName);
/// var_dump($d->load('mem://none'));
/// var_dump($d->load(__DIR__ . '/none.xml'));
/// var_dump($d->save('mem://out'), Mem::$data['out']);
/// var_dump($d->save('a%00b'));
/// ```
/// ```expect
/// bool(true)
/// string(1) "a"
///
/// Warning: DOMDocument::load(): I/O warning : failed to load external entity "mem://none" in %s on line 13
/// bool(false)
///
/// Warning: DOMDocument::load(): I/O warning : failed to load external entity "%s/none.xml" in %s on line 14
/// bool(false)
/// int(34)
/// string(34) "<?xml version="1.0"?>
/// <r><a/></r>
/// "
///
/// Warning: DOMDocument::save(): URI must not contain percent-encoded NUL bytes in %s on line 16
/// bool(false)
/// ```
///
/// The stream is libxml's, so `fclose()` refuses it
/// (`PHP_STREAM_FLAG_NO_FCLOSE`):
///
/// ```php
/// $f = sys_get_temp_dir() . '/libxml_no_fclose.xml';
/// $w = new XMLWriter; $w->openUri($f); $r = get_resources();
/// var_dump(fclose(end($r))); unset($w); unlink($f);
/// ```
/// ```expect
///
/// Warning: fclose(): cannot close the provided stream, as it must not be manually closed in %s on line 4
/// bool(false)
/// ```
fn open(
    req: &mut Request,
    filename: &[u8],
    mode: &[u8],
    read_only: bool,
) -> Result<Option<Owner<Stream>>, Thrown> {
    if contains_nul_escape(filename) {
        docref(
            req,
            Level::Warning,
            "URI must not contain percent-encoded NUL bytes",
        );
        return Ok(None);
    }
    let local = match Uri::parse(filename) {
        Some(uri) if uri.scheme.as_ref().is_none_or(|s| s.starts_with(b"file")) => {
            Uri::unescape(filename)
        }
        _ => Some(filename.to_vec()),
    };
    let Some(path) = local else {
        return Ok(None);
    };
    if read_only && req.wrapper_url_stat(&path)? == UrlStat::Failed {
        return Ok(None);
    }
    // `php_stream_context_from_zval(LIBXML(stream_context), 0)`: the
    // context `libxml_set_streams_context` kept, the default one else.
    let kept = req
        .state::<State>()
        .stream_context
        .as_ref()
        .and_then(|v| v.as_resource());
    let context = match kept {
        Some(context) => StreamCtx::fetch(req, context)?,
        None => StreamCtx::default_ctx(req)?,
    };
    let opened = req.with_stream_ctx(Some(context), |req| req.open_stream(&path, mode, false))?;
    // "Prevent from closing this by fclose()": the stream is libxml's.
    if let Some(stream) = &opened {
        stream.forbid_fclose(req);
    }
    Ok(opened)
}
