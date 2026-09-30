//! `zlib.c`'s incremental natives: `DeflateContext` and `InflateContext`,
//! `inflate_context_object_handlers` and `deflate_context_object_handlers`
//! (refusing `new`, `clone` and comparison), and `deflate_init`,
//! `deflate_add`, `inflate_init`, `inflate_add`, `inflate_get_status` and
//! `inflate_get_read_len` with their buffer growth copied, since it
//! decides when `inflate_add` asks zlib for more room.

use std::cmp::Ordering;

use php_ext::abi::{CloneObj, Instance, ObjectHandlers, OrFalse, Raw, Union};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{Function, lookup_class, new_object};
use php_ext::coerce::{to_long, to_string, value_name};
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpObject, PhpString, Value, ValueRef};

use crate::constants::{ZLIB_ENCODING_DEFLATE, ZLIB_ENCODING_GZIP, ZLIB_ENCODING_RAW};
use crate::encode::size_guess;
use crate::ffi::{Deflate, Flush, Inflate, Status};

/// `CHUNK_SIZE` of `inflate_add`: its least buffer and its step.
const CHUNK_SIZE: usize = 8192;

/// A `DeflateContext`'s `php_zlib_context`: the stream `deflate_init`
/// made, which only it makes.
#[derive(Default)]
pub(crate) struct Deflating(Option<Deflate>);

/// An `InflateContext`'s `php_zlib_context`: the stream, the dictionary
/// not yet handed to it, and the status of the last `inflate` call,
/// which `inflate_get_status` answers.
pub(crate) struct Inflating {
    stream: Option<Inflate>,
    dictionary: Option<Vec<u8>>,
    status: Status,
}

impl Default for Inflating {
    fn default() -> Inflating {
        Inflating {
            stream: None,
            dictionary: None,
            status: Status::Ok,
        }
    }
}

/// `final class InflateContext`, `@strict-properties`,
/// `@not-serializable`, under [`INFLATE`].
///
/// ```php
/// try { new InflateContext; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $c = inflate_init(ZLIB_ENCODING_RAW);
/// try { clone $c; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($c); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { $c->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($c == inflate_init(ZLIB_ENCODING_RAW));
/// ```
/// ```expect
/// Cannot directly construct InflateContext, use inflate_init() instead
/// Trying to clone an uncloneable object of class InflateContext
/// Serialization of 'InflateContext' is not allowed
/// Cannot create dynamic property InflateContext::$x
/// bool(false)
/// ```
#[php_class(
    final,
    not_serializable,
    size = 232, payload = Inflating,
    handlers = INFLATE,
    dynamic_properties = forbidden
)]
pub(crate) struct InflateContext;

#[php_class]
impl InflateContext {}

/// `final class DeflateContext`, as [`InflateContext`].
///
/// ```php
/// try { new DeflateContext; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { clone deflate_init(ZLIB_ENCODING_RAW); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot directly construct DeflateContext, use deflate_init() instead
/// Trying to clone an uncloneable object of class DeflateContext
/// ```
#[php_class(
    final,
    not_serializable,
    size = 232, payload = Deflating,
    handlers = DEFLATE,
    dynamic_properties = forbidden
)]
pub(crate) struct DeflateContext;

#[php_class]
impl DeflateContext {}

/// The two classes' handlers, `inflate_context_object_handlers` and
/// `deflate_context_object_handlers`: `std_object_handlers` with
/// `get_constructor` refusing in the words of the class's own init,
/// `clone_obj` php's null slot and `compare`
/// `zend_objects_not_comparable`.
pub(crate) struct Handlers(&'static str);

pub(crate) static INFLATE: Handlers =
    Handlers("Cannot directly construct InflateContext, use inflate_init() instead");
pub(crate) static DEFLATE: Handlers =
    Handlers("Cannot directly construct DeflateContext, use deflate_init() instead");

impl ObjectHandlers for Handlers {
    fn get_constructor(
        &self,
        req: &mut Request,
        _object: PhpObject,
    ) -> Result<Option<Function>, Thrown> {
        Err(throw(req, "Error", self.0))
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }

    /// `deflate_context_free_obj` and `inflate_context_free_obj`: the
    /// stream ended, its state given back to the request's heap.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        if let Some(stream) = object
            .native_mut::<Deflating>(req)
            .and_then(|payload| payload.0.take())
        {
            stream.end(req);
        }
        if let Some(stream) = object
            .native_mut::<Inflating>(req)
            .and_then(|payload| payload.stream.take())
        {
            stream.end(req);
        }
    }

    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let _ = (req, left, right);
        Ok(None)
    }
}

/// The `$options` of `deflate_init` and `inflate_init`, php's `H`: an
/// array, or an object's property table.
struct Options(Value);

impl Options {
    /// `zend_hash_str_find(options, name)`, dereferenced.
    ///
    /// ```php
    /// $w = deflate_init(ZLIB_ENCODING_RAW, ['level' => 1, 'window' => 9]); unset($w);
    /// $m = memory_get_usage();
    /// $d = deflate_init(ZLIB_ENCODING_RAW, ['level' => 1, 'window' => 9]); unset($d);
    /// var_dump(memory_get_usage() - $m);
    /// ```
    /// ```expect
    /// int(0)
    /// ```
    fn find(&self, req: &mut Request, name: &[u8]) -> Result<Option<Value>, Thrown> {
        match self.0.as_ref() {
            ValueRef::Array(array) => {
                // A key for the lookup alone, given back once it is made,
                // as `zend_hash_str_find` allocates none.
                let key = PhpString::new(req, name)?;
                let found = array.get(&Key::Str(key)).map(|value| value.deref());
                req.release(Value::string(key));
                Ok(found)
            }
            ValueRef::Object(object) => {
                let object = *object;
                Ok(object
                    .entries(req)?
                    .into_iter()
                    .find(|(key, _)| matches!(key, Key::Str(k) if k.as_bytes() == name))
                    .map(|(_, value)| value.deref()))
            }
            _ => Ok(None),
        }
    }

    /// An option as `zval_get_long` reads it, `default` where it is
    /// absent.
    fn long(&self, req: &mut Request, name: &[u8], default: i64) -> Result<i64, Thrown> {
        match self.find(req, name)? {
            Some(value) => to_long(req, &value),
            None => Ok(default),
        }
    }

    /// `zlib_create_dictionary_string`: the `dictionary` option, a
    /// string as it is, or an array's strings each ended by a NUL; an
    /// empty array is none.
    fn dictionary(&self, req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
        let Some(value) = self.find(req, b"dictionary")? else {
            return Ok(None);
        };
        match value.as_ref() {
            ValueRef::Str(s) => Ok(Some(s.as_bytes().to_vec())),
            ValueRef::Array(array) => {
                let mut out = Vec::new();
                for (_, element) in array {
                    let string = to_string(req, &element)?;
                    let bytes = string.as_bytes();
                    if bytes.is_empty() {
                        return Err(arg_value_error(req, 2, "not contain empty strings"));
                    }
                    if bytes.contains(&0) {
                        return Err(arg_value_error(
                            req,
                            2,
                            "not contain strings with null bytes",
                        ));
                    }
                    out.extend_from_slice(bytes);
                    out.push(0);
                }
                Ok((!out.is_empty()).then_some(out))
            }
            _ => {
                let given = value_name(req, &value);
                Err(arg_type_error(
                    req,
                    2,
                    &format!("be of type zero-terminated string or array, {given} given"),
                ))
            }
        }
    }
}

/// Whether `encoding` is one of the three `ZLIB_ENCODING_*`.
fn known(encoding: i64) -> bool {
    [ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, ZLIB_ENCODING_DEFLATE].contains(&encoding)
}

/// The window bits an encoding asks zlib for under a `window` below 15:
/// raw deflate's negative count raised, the others lowered.
fn windowed(encoding: i64, window: i64) -> i64 {
    if encoding < 0 {
        encoding + (15 - window)
    } else {
        encoding - (15 - window)
    }
}

/// A new object of `class` holding what `fill` lays in its payload.
fn made<P: 'static + Send>(
    req: &mut Request,
    class: &[u8],
    fill: impl FnOnce(&mut P),
) -> Result<PhpObject, Thrown> {
    let Some(id) = lookup_class(req, class)? else {
        let text = format!("Class \"{}\" not found", String::from_utf8_lossy(class));
        return Err(throw(req, "Error", &text));
    };
    let object = new_object(req, id)?;
    if let Some(payload) = object.native_mut::<P>(req) {
        fill(payload);
    }
    Ok(object)
}

/// `deflate_init(int $encoding, array|object $options = []):
/// DeflateContext|false`: `level`, `memory`, `window` and `strategy`
/// checked in that order, then the encoding, then the dictionary, which
/// is set at once.
///
/// ```php
/// $c = deflate_init(ZLIB_ENCODING_DEFLATE, ['level' => 9, 'memory' => 4, 'window' => 10, 'strategy' => ZLIB_FILTERED]);
/// echo bin2hex(deflate_add($c, str_repeat('ab', 50), ZLIB_FINISH)), "\n";
/// $c = deflate_init(ZLIB_ENCODING_RAW, ['dictionary' => ['ab', 'cd']]);
/// echo bin2hex(deflate_add($c, 'abcdabcd', ZLIB_FINISH)), "\n";
/// $o = new stdClass; $o->level = 1;
/// echo bin2hex(deflate_add(deflate_init(ZLIB_ENCODING_GZIP, $o), 'xyz', ZLIB_FINISH)), "\n";
/// foreach ([['level' => 10], ['memory' => 0], ['window' => 16], ['strategy' => 9], ['dictionary' => ['']], ['dictionary' => ["a\0"]], ['dictionary' => 1]] as $opt) {
///     try { deflate_init(ZLIB_ENCODING_RAW, $opt); } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// }
/// try { deflate_init(42); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 28cf4b4c4aa4390400840b2617
/// 4b4c4a4e016100
/// 1f8b0800000000000413aba8ac020067ba8eeb03000000
/// ValueError: deflate_init(): "level" option must be between -1 and 9
/// ValueError: deflate_init(): "memory" option must be between 1 and 9
/// ValueError: deflate_init(): "window" option must be between 8 and 15
/// ValueError: deflate_init(): "strategy" option must be one of ZLIB_FILTERED, ZLIB_HUFFMAN_ONLY, ZLIB_RLE, ZLIB_FIXED, or ZLIB_DEFAULT_STRATEGY
/// ValueError: deflate_init(): Argument #2 ($options) must not contain empty strings
/// ValueError: deflate_init(): Argument #2 ($options) must not contain strings with null bytes
/// TypeError: deflate_init(): Argument #2 ($options) must be of type zero-terminated string or array, int given
/// deflate_init(): Argument #1 ($encoding) must be one of ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, or ZLIB_ENCODING_DEFLATE
/// ```
#[php_function]
fn deflate_init(
    req: &mut Request,
    encoding: i64,
    #[php(default = [])] options: Raw<Union<PhpObject, php_ext::values::PhpArray>>,
) -> Result<OrFalse<Instance<DeflateContext>>, Thrown> {
    let options = Options(options.value());
    let level = options.long(req, b"level", -1)?;
    if !(-1..=9).contains(&level) {
        return Err(throw(
            req,
            "ValueError",
            "deflate_init(): \"level\" option must be between -1 and 9",
        ));
    }
    let memory = options.long(req, b"memory", 8)?;
    if !(1..=9).contains(&memory) {
        return Err(throw(
            req,
            "ValueError",
            "deflate_init(): \"memory\" option must be between 1 and 9",
        ));
    }
    let window = options.long(req, b"window", 15)?;
    if !(8..=15).contains(&window) {
        return Err(throw(
            req,
            "ValueError",
            "deflate_init(): \"window\" option must be between 8 and 15",
        ));
    }
    let strategy = options.long(req, b"strategy", 0)?;
    if !(0..=4).contains(&strategy) {
        return Err(throw(
            req,
            "ValueError",
            "deflate_init(): \"strategy\" option must be one of ZLIB_FILTERED, ZLIB_HUFFMAN_ONLY, ZLIB_RLE, ZLIB_FIXED, or ZLIB_DEFAULT_STRATEGY",
        ));
    }
    if !known(encoding) {
        return Err(arg_value_error(
            req,
            1,
            "be one of ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, or ZLIB_ENCODING_DEFLATE",
        ));
    }
    let dictionary = options.dictionary(req)?;
    let Ok(mut stream) = Deflate::new(req, level, windowed(encoding, window), memory, strategy)?
    else {
        docref(
            req,
            Level::Warning,
            "Failed allocating zlib.deflate context",
        );
        return Ok(OrFalse::False);
    };
    if let Some(dictionary) = dictionary {
        stream.set_dictionary(req, &dictionary)?;
    }
    let object = made::<Deflating>(req, b"DeflateContext", |payload| payload.0 = Some(stream))?;
    Ok(OrFalse::Value(Instance::of(object)))
}

/// `deflate_add(DeflateContext $context, string $data, int $flush_mode =
/// ZLIB_SYNC_FLUSH): string|false`: a buffer of [`size_guess`], at
/// least 64, growing by 64 while zlib fills it; the stream reset once it
/// ends, so the `DeflateContext` writes the next one.
///
/// ```php
/// $c = deflate_init(ZLIB_ENCODING_GZIP);
/// $out = deflate_add($c, 'Hello, ', ZLIB_NO_FLUSH) . deflate_add($c, 'world') . deflate_add($c, '!', ZLIB_FINISH);
/// echo bin2hex($out), "\n", gzdecode($out), "\n";
/// var_dump(deflate_add($c, ''), bin2hex(deflate_add($c, '', ZLIB_FINISH)), bin2hex(deflate_add($c, 'x', ZLIB_BLOCK)));
/// $big = deflate_add(deflate_init(ZLIB_ENCODING_RAW, ['level' => 0]), random_bytes(1) . str_repeat("\x9c", 100000), ZLIB_FINISH);
/// var_dump(strlen($big));
/// try { deflate_add($c, 'x', 42); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 1f8b0800000000000013f248cdc9c9d75128cf2fca4901000000ffff530400e6c6e6eb0d000000
/// Hello, world!
/// string(0) ""
/// string(40) "1f8b080000000000001303000000000000000000"
/// string(24) "1f8b0800000000000013aa00"
/// int(100011)
/// deflate_add(): Argument #3 ($flush_mode) must be one of ZLIB_NO_FLUSH, ZLIB_PARTIAL_FLUSH, ZLIB_SYNC_FLUSH, ZLIB_FULL_FLUSH, ZLIB_BLOCK, or ZLIB_FINISH
/// ```
#[php_function]
fn deflate_add(
    req: &mut Request,
    context: Instance<DeflateContext>,
    data: PhpString,
    #[php(default = ZLIB_SYNC_FLUSH)] flush_mode: i64,
) -> Result<OrFalse<PhpString>, Thrown> {
    let Some(flush) = Flush::of(flush_mode) else {
        return Err(arg_value_error(
            req,
            3,
            "be one of ZLIB_NO_FLUSH, ZLIB_PARTIAL_FLUSH, ZLIB_SYNC_FLUSH, ZLIB_FULL_FLUSH, ZLIB_BLOCK, or ZLIB_FINISH",
        ));
    };
    let input = data.as_bytes();
    if input.is_empty() && flush != Flush::Finish {
        return Ok(OrFalse::Value(PhpString::new(req, b"")?));
    }
    let mut out = vec![0; size_guess(input.len()).max(64)];
    let (mut read, mut used) = (0, 0);
    let Some(mut stream) = context.payload_mut(req)?.0.take() else {
        return Err(throw(req, "Error", "DeflateContext has no stream"));
    };
    let status = loop {
        if used == out.len() {
            out.resize(out.len() + 64, 0);
        }
        let step = match stream.deflate(req, &input[read..], &mut out[used..], flush) {
            Ok(step) => step,
            Err(thrown) => {
                context.payload_mut(req)?.0 = Some(stream);
                return Err(thrown);
            }
        };
        read += step.read;
        used += step.written;
        if step.status != Status::Ok || used != out.len() {
            if step.status == Status::StreamEnd
                && let Err(thrown) = stream.reset(req)
            {
                context.payload_mut(req)?.0 = Some(stream);
                return Err(thrown);
            }
            break step.status;
        }
    };
    context.payload_mut(req)?.0 = Some(stream);
    match status {
        Status::Ok | Status::StreamEnd => {
            out.truncate(used);
            Ok(OrFalse::Value(PhpString::new(req, &out)?))
        }
        failed => {
            let text = format!("zlib error ({})", failed.message());
            docref(req, Level::Warning, &text);
            Ok(OrFalse::False)
        }
    }
}

/// `inflate_init(int $encoding, array|object $options = []):
/// InflateContext|false`: `window` checked, then the encoding, then the
/// dictionary, which raw deflate is handed at once and the others when
/// zlib asks.
///
/// ```php
/// $c = inflate_init(ZLIB_ENCODING_DEFLATE, ['window' => 9]);
/// var_dump(inflate_add($c, gzcompress('hello'), ZLIB_FINISH));
/// try { inflate_init(ZLIB_ENCODING_RAW, ['window' => 7]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { inflate_init(1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(inflate_init(ZLIB_ENCODING_RAW, ['dictionary' => 'abc']) instanceof InflateContext);
/// ```
/// ```expect
/// Warning: inflate_add(): data error in %s on line 3
/// bool(false)
/// zlib window size (logarithm) (7) must be within 8..15
/// Encoding mode must be ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP or ZLIB_ENCODING_DEFLATE
/// bool(true)
/// ```
#[php_function]
fn inflate_init(
    req: &mut Request,
    encoding: i64,
    #[php(default = [])] options: Raw<Union<PhpObject, php_ext::values::PhpArray>>,
) -> Result<OrFalse<Instance<InflateContext>>, Thrown> {
    let options = Options(options.value());
    let window = options.long(req, b"window", 15)?;
    if !(8..=15).contains(&window) {
        let text = format!("zlib window size (logarithm) ({window}) must be within 8..15");
        return Err(throw(req, "ValueError", &text));
    }
    if !known(encoding) {
        return Err(throw(
            req,
            "ValueError",
            "Encoding mode must be ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP or ZLIB_ENCODING_DEFLATE",
        ));
    }
    let mut dictionary = options.dictionary(req)?;
    let bits = windowed(encoding, window);
    let Ok(mut stream) = Inflate::new(req, bits)? else {
        docref(
            req,
            Level::Warning,
            "Failed allocating zlib.inflate context",
        );
        return Ok(OrFalse::False);
    };
    if bits == ZLIB_ENCODING_RAW
        && let Some(dict) = dictionary.take_if(|dict| !dict.is_empty())
        && stream.set_dictionary(req, &dict)? == Status::DataError
    {
        docref(
            req,
            Level::Warning,
            "Dictionary does not match expected dictionary (incorrect adler32 hash)",
        );
    }
    let object = made::<Inflating>(req, b"InflateContext", |payload| {
        payload.stream = Some(stream);
        payload.dictionary = dictionary;
    })?;
    Ok(OrFalse::Value(Instance::of(object)))
}

/// What one `inflate_add` call came to: the bytes, or php's warning.
enum Inflated {
    Out(Vec<u8>),
    Refused(String),
}

/// `inflate_add(InflateContext $context, string $data, int $flush_mode =
/// ZLIB_SYNC_FLUSH): string|false`: the stream reset lazily after it
/// ended, so `inflate_get_read_len` still reads the last one's length; a
/// buffer of the input's length, at least [`CHUNK_SIZE`], growing by it
/// while zlib fills it; the dictionary handed over when zlib asks.
///
/// ```php
/// $c = inflate_init(ZLIB_ENCODING_GZIP);
/// $gz = gzencode(str_repeat('0123456789', 2000));
/// $out = '';
/// foreach (str_split($gz, 100) as $piece) $out .= inflate_add($c, $piece, ZLIB_NO_FLUSH);
/// var_dump(strlen($out), inflate_get_status($c), inflate_get_read_len($c));
/// var_dump(inflate_add($c, gzencode('again'), ZLIB_FINISH), inflate_get_status($c));
/// $d = inflate_init(ZLIB_ENCODING_DEFLATE);
/// var_dump(inflate_add($d, 'garbage'));
/// $w = deflate_init(ZLIB_ENCODING_DEFLATE, ['dictionary' => 'abcd']);
/// $z = deflate_add($w, 'abcdabcd', ZLIB_FINISH);
/// var_dump(inflate_add(inflate_init(ZLIB_ENCODING_DEFLATE, ['dictionary' => 'abcd']), $z));
/// var_dump(inflate_add(inflate_init(ZLIB_ENCODING_DEFLATE), $z));
/// var_dump(inflate_add(inflate_init(ZLIB_ENCODING_DEFLATE, ['dictionary' => 'wxyz']), $z));
/// ```
/// ```expect
/// int(20000)
/// int(1)
/// int(85)
/// string(5) "again"
/// int(1)
///
/// Warning: inflate_add(): data error in %s on line 9
/// bool(false)
/// string(8) "abcdabcd"
///
/// Warning: inflate_add(): Inflating this data requires a preset dictionary, please specify it in the options array of inflate_init() in %s on line 13
/// bool(false)
///
/// Warning: inflate_add(): Dictionary does not match expected dictionary (incorrect adler32 hash) in %s on line 14
/// bool(false)
/// ```
#[php_function]
fn inflate_add(
    req: &mut Request,
    context: Instance<InflateContext>,
    data: PhpString,
    #[php(default = ZLIB_SYNC_FLUSH)] flush_mode: i64,
) -> Result<OrFalse<PhpString>, Thrown> {
    let Some(flush) = Flush::of(flush_mode) else {
        return Err(arg_value_error(
            req,
            3,
            "be one of ZLIB_NO_FLUSH, ZLIB_PARTIAL_FLUSH, ZLIB_SYNC_FLUSH, ZLIB_FULL_FLUSH, ZLIB_BLOCK, or ZLIB_FINISH",
        ));
    };
    let input = data.as_bytes();
    let payload = context.payload_mut(req)?;
    let Some(mut stream) = payload.stream.take() else {
        return Err(throw(req, "Error", "InflateContext has no stream"));
    };
    let (mut dictionary, mut status) = (payload.dictionary.take(), payload.status);
    let inflated = (|| {
        if status == Status::StreamEnd {
            status = Status::Ok;
            stream.reset(req)?;
        }
        if input.is_empty() && flush != Flush::Finish {
            Ok(Inflated::Out(Vec::new()))
        } else {
            inflate_rounds(req, &mut stream, &mut dictionary, &mut status, input, flush)
        }
    })();
    let payload = context.payload_mut(req)?;
    payload.stream = Some(stream);
    payload.dictionary = dictionary;
    payload.status = status;
    let inflated = inflated?;
    match inflated {
        Inflated::Out(out) => Ok(OrFalse::Value(PhpString::new(req, &out)?)),
        Inflated::Refused(text) => {
            docref(req, Level::Warning, &text);
            Ok(OrFalse::False)
        }
    }
}

/// `inflate_add`'s loop.
fn inflate_rounds(
    req: &mut Request,
    stream: &mut Inflate,
    dictionary: &mut Option<Vec<u8>>,
    last: &mut Status,
    input: &[u8],
    flush: Flush,
) -> Result<Inflated, Thrown> {
    let mut out = vec![0; input.len().max(CHUNK_SIZE)];
    let (mut read, mut used) = (0, 0);
    loop {
        let step = stream.inflate(req, &input[read..], &mut out[used..], flush)?;
        read += step.read;
        used += step.written;
        *last = step.status;
        let full = used == out.len();
        match step.status {
            Status::Ok | Status::BufError
                if full && (step.status == Status::Ok || flush == Flush::Finish) =>
            {
                out.resize(out.len() + CHUNK_SIZE, 0);
            }
            Status::Ok | Status::BufError | Status::StreamEnd => break,
            Status::NeedDict => match dictionary.take() {
                Some(dict) => {
                    if stream.set_dictionary(req, &dict)? == Status::DataError {
                        return Ok(Inflated::Refused(
                            "Dictionary does not match expected dictionary (incorrect adler32 hash)".into(),
                        ));
                    }
                }
                None => {
                    return Ok(Inflated::Refused(
                        "Inflating this data requires a preset dictionary, please specify it in the options array of inflate_init()".into(),
                    ));
                }
            },
            failed => return Ok(Inflated::Refused(failed.message())),
        }
    }
    out.truncate(used);
    Ok(Inflated::Out(out))
}

/// `inflate_get_status(InflateContext $context): int`: the last
/// `inflate` call's status, `ZLIB_OK` before any.
///
/// ```php
/// $c = inflate_init(ZLIB_ENCODING_RAW);
/// var_dump(inflate_get_status($c));
/// inflate_add($c, gzdeflate('abc'));
/// var_dump(inflate_get_status($c));
/// ```
/// ```expect
/// int(0)
/// int(1)
/// ```
#[php_function]
fn inflate_get_status(req: &mut Request, context: Instance<InflateContext>) -> Result<i64, Thrown> {
    Ok(context.payload_mut(req)?.status.code())
}

/// `inflate_get_read_len(InflateContext $context): int`: `Z.total_in`.
///
/// ```php
/// $c = inflate_init(ZLIB_ENCODING_DEFLATE);
/// $z = gzcompress('hello world');
/// inflate_add($c, $z . 'trailing');
/// var_dump(strlen($z), inflate_get_read_len($c));
/// ```
/// ```expect
/// int(19)
/// int(19)
/// ```
#[php_function]
fn inflate_get_read_len(
    req: &mut Request,
    context: Instance<InflateContext>,
) -> Result<i64, Thrown> {
    let payload = context.payload_mut(req)?;
    Ok(payload.stream.as_ref().map_or(0, |stream| {
        i64::try_from(stream.total_in()).unwrap_or(i64::MAX)
    }))
}
