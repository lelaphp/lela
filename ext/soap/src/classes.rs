//! The value classes of `soap.c`: `SoapParam`, `SoapHeader`, `SoapFault`,
//! `SoapVar`, and the two opaque handles `Soap\Url` and `Soap\Sdl` a
//! client keeps its connection URL and its description in; with
//! `set_soap_fault` and `add_soap_fault`, which every fault is made by.

use php_ext::abi::ConstValue;
use php_ext::abi::{ObjectHandlers, Passed, Refusals, Std, This, Union};
use php_ext::attr::php_class;
use php_ext::call::{call_method, read_prop};
use php_ext::coerce::{to_long, to_string};
use php_ext::raising::{Thrown, arg_value_error, argument_error};
use php_ext::request::Request;
use php_ext::values::{Emalloc, Owner, PhpArray, PhpObject, PhpString, TypeTag, Value, ValueRef};

use crate::model;
use crate::throwables::Throwables;
use crate::{SOAP_1_1, SOAP_1_2, State};

type R<T> = Result<T, Thrown>;

/// `instanceof_function(ce, soap_fault_class_entry)`.
pub(crate) fn is_fault(req: &mut Request, object: PhpObject) -> bool {
    match Throwables::of(req) {
        Ok(ce) => object.instance_of(req, ce.soap_fault),
        Err(_) => false,
    }
}

/// A string value, made in the request's heap.
pub(crate) fn string(req: &mut Request, bytes: &[u8]) -> R<Owner<Value>> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

// ---- SoapParam

/// `SoapParam`: a value sent under a name of its own.
///
/// ```php
/// $p = new SoapParam([1, 2], 'list');
/// var_dump($p);
/// try { new SoapParam(1, ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(SoapParam)#1 (2) {
///   ["param_name"]=>
///   string(4) "list"
///   ["param_data"]=>
///   array(2) {
///     [0]=>
///     int(1)
///     [1]=>
///     int(2)
///   }
/// }
/// SoapParam::__construct(): Argument #2 ($name) must not be empty
/// ```
#[php_class]
pub(crate) struct SoapParam;

#[php_class]
impl SoapParam {
    #[php(property)]
    const param_name: PhpString;
    #[php(property)]
    const param_data: Value;

    /// `SoapParam::__construct(mixed $data, string $name)`.
    fn __construct(req: &mut Request, this: This<Self>, data: Value, name: PhpString) -> R<()> {
        if name.is_empty() {
            return Err(arg_value_error(req, 2, "not be empty"));
        }
        let object = this.object();
        let name = Owner::take(req, Value::string(name));
        object.set_slot(req, 0, name)?;
        let data = Owner::take(req, data.deref());
        object.set_slot(req, 1, data)
    }
}

// ---- SoapHeader

/// `SoapHeader`: one header block a client sends or a server answers.
///
/// ```php
/// $h = new SoapHeader('urn:x', 'auth', ['u' => 'me'], true, SOAP_ACTOR_NEXT);
/// var_dump($h->namespace, $h->name, $h->data, $h->mustUnderstand, $h->actor);
/// var_dump((new SoapHeader('urn:x', 'h', null, false, 'http://a'))->actor);
/// try { (new SoapHeader('urn:x', 'h'))->actor; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// foreach ([['', 'h'], ['n', ''], ['n', 'h', 1, false, 'ab'], ['n', 'h', 1, false, 7]] as $a) {
///     try { new SoapHeader(...$a); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// var_dump((new SoapHeader('n', 'h'))->data);
/// ```
/// ```expect
/// string(5) "urn:x"
/// string(4) "auth"
/// array(1) {
///   ["u"]=>
///   string(2) "me"
/// }
/// bool(true)
/// int(1)
/// string(8) "http://a"
/// Typed property SoapHeader::$actor must not be accessed before initialization
/// SoapHeader::__construct(): Argument #1 ($namespace) must not be empty
/// SoapHeader::__construct(): Argument #2 ($name) must not be empty
/// SoapHeader::__construct(): Argument #5 ($actor) must be longer than 2 characters
/// SoapHeader::__construct(): Argument #5 ($actor) must be one of SOAP_ACTOR_NEXT, SOAP_ACTOR_NONE, or SOAP_ACTOR_UNLIMATERECEIVER
/// NULL
/// ```
#[php_class]
pub(crate) struct SoapHeader;

#[php_class]
impl SoapHeader {
    #[php(property)]
    const namespace: PhpString;
    #[php(property)]
    const name: PhpString;
    #[php(property)]
    const data: Value = ConstValue::Null;
    #[php(property)]
    const mustUnderstand: bool;
    #[php(property)]
    const actor: Option<Union<PhpString, i64>>;

    /// `SoapHeader::__construct(string $namespace, string $name, mixed $data
    /// = UNKNOWN, bool $mustUnderstand = false, string|int|null $actor = null)`.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        namespace: PhpString,
        name: PhpString,
        data: Passed<Value>,
        #[php(default = false)] mustUnderstand: bool,
        #[php(default = null)] actor: Option<Union<PhpString, i64>>,
    ) -> R<()> {
        if namespace.is_empty() {
            return Err(arg_value_error(req, 1, "not be empty"));
        }
        if name.is_empty() {
            return Err(arg_value_error(req, 2, "not be empty"));
        }
        let object = this.object();
        let namespace = Owner::take(req, Value::string(namespace));
        object.set_slot(req, 0, namespace)?;
        let name = Owner::take(req, Value::string(name));
        object.set_slot(req, 1, name)?;
        if let Passed::Given(data) = data {
            let data = Owner::take(req, data.deref());
            object.set_slot(req, 2, data)?;
        }
        object.set_slot(req, 3, Owner::bool(mustUnderstand))?;
        match actor {
            Some(Union::A(s)) => {
                if s.len() > 2 {
                    let s = Owner::take(req, Value::string(s));
                    object.set_slot(req, 4, s)?;
                } else {
                    return Err(arg_value_error(req, 5, "be longer than 2 characters"));
                }
            }
            Some(Union::B(n)) => {
                if (1..=3).contains(&n) {
                    object.set_slot(req, 4, Owner::int(n))?;
                } else {
                    return Err(arg_value_error(
                        req,
                        5,
                        "be one of SOAP_ACTOR_NEXT, SOAP_ACTOR_NONE, or SOAP_ACTOR_UNLIMATERECEIVER",
                    ));
                }
            }
            None => {}
        }
        Ok(())
    }
}

// ---- SoapFault

/// `zend_ce_exception->default_properties_count`: where `SoapFault`'s own
/// slots start.
const FAULT_START: u32 = 7;

/// `SoapFault`'s eight slots, after `Exception`'s.
#[derive(Clone, Copy)]
pub(crate) enum FaultSlot {
    String = 0,
    Code = 1,
    CodeNs = 2,
    Actor = 3,
    Detail = 4,
    Name = 5,
    HeaderFault = 6,
    Lang = 7,
}

impl FaultSlot {
    pub(crate) fn index(self) -> u32 {
        FAULT_START + self as u32
    }
}

fn set_fault_slot(
    req: &mut Request,
    object: PhpObject,
    slot: FaultSlot,
    value: Owner<Value>,
) -> R<()> {
    object.set_slot(req, slot.index(), value)
}

/// `SoapFault`: a fault a server answers and a client throws.
///
/// ```php
/// $f = new SoapFault('Server', 'Oops', 'actor', ['d' => 1], 'name', 'hf', 'nl');
/// var_dump($f->faultcode, $f->faultcodens, $f->faultstring, $f->getMessage(), $f->faultactor, $f->detail, $f->_name, $f->headerfault, $f->lang);
/// $g = new SoapFault(['urn:x', 'Mine'], 'two');
/// var_dump($g->faultcode, $g->faultcodens, (new SoapFault(null, 's'))->faultcode, (new SoapFault('Other', 's'))->faultcodens);
/// foreach (['', ['a'], ['a', 1]] as $c) {
///     try { new SoapFault($c, 'x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// var_dump((new SoapFault('Client', 'x', null, null, ''))->_name);
/// ```
/// ```expect
/// string(6) "Server"
/// string(41) "http://schemas.xmlsoap.org/soap/envelope/"
/// string(4) "Oops"
/// string(4) "Oops"
/// string(5) "actor"
/// array(1) {
///   ["d"]=>
///   int(1)
/// }
/// string(4) "name"
/// string(2) "hf"
/// string(2) "en"
/// string(4) "Mine"
/// string(5) "urn:x"
/// NULL
/// NULL
/// SoapFault::__construct(): Argument #1 ($code) is not a valid fault code
/// SoapFault::__construct(): Argument #1 ($code) is not a valid fault code
/// SoapFault::__construct(): Argument #1 ($code) is not a valid fault code
/// NULL
/// ```
#[php_class(extends(crate::Exception))]
pub(crate) struct SoapFault;

#[php_class]
impl SoapFault {
    #[php(property)]
    const faultstring: PhpString;
    #[php(property)]
    const faultcode: Option<PhpString> = None;
    #[php(property)]
    const faultcodens: Option<PhpString> = None;
    #[php(property)]
    const faultactor: Option<PhpString> = None;
    #[php(property)]
    const detail: Value = ConstValue::Null;
    #[php(property)]
    const _name: Option<PhpString> = None;
    #[php(property)]
    const headerfault: Value = ConstValue::Null;
    #[php(property)]
    const lang: PhpString = "";

    /// `SoapFault::__construct(array|string|null $code, string $string,
    /// ?string $actor = null, mixed $details = null, ?string $name = null,
    /// mixed $headerFault = null, string $lang = "")`: the properties
    /// cleared and set again, the language always `en`. `Exception::$message`
    /// is `$faultstring`'s string and `$_name` the argument's, one more
    /// owner of each, where every other string is `ZVAL_STRING`'s copy,
    /// made even for the empty string and one byte. A fault made first
    /// leaves `soap_lang_en` standing, which php interns at `PHP_MINIT`.
    /// Each line is the fault's bytes past an `Exception`'s, whose
    /// `$file` and trace take what the file's path takes:
    ///
    /// ```php
    /// $w = new SoapFault('Server', 'first');
    /// $m = memory_get_usage(); $e = new Exception('Nope Here'); $x = memory_get_usage() - $m;
    /// $m = memory_get_usage(); $f = new SoapFault('HTTP', 'Nope Here'); echo memory_get_usage() - $m - $x, "\n";
    /// $m = memory_get_usage(); $g = new SoapFault('X', '', 'actor', null, 'nm'); echo memory_get_usage() - $m - $x, "\n";
    /// $m = memory_get_usage(); $h = new SoapFault(['urn:x', 'C'], 'Nope Here'); echo memory_get_usage() - $m - $x, "\n";
    /// ```
    /// ```expect
    /// 232
    /// 256
    /// 264
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        code: Option<Union<PhpArray, PhpString>>,
        string: PhpString,
        #[php(default = null)] actor: Option<PhpString>,
        #[php(default = null)] details: Value,
        #[php(default = null)] name: Option<PhpString>,
        #[php(default = null)] headerFault: Value,
        #[php(default = "")] lang: php_ext::abi::Path,
    ) -> R<()> {
        let _ = lang;
        let mut fault_code: Option<Vec<u8>> = None;
        let mut fault_code_ns: Option<Vec<u8>> = None;
        let given = code.is_some();
        match &code {
            Some(Union::B(s)) => fault_code = Some(s.as_bytes().to_vec()),
            Some(Union::A(a)) if a.len() == 2 => {
                let ns = a.get(&php_ext::values::Key::Int(0));
                let c = a.get(&php_ext::values::Key::Int(1));
                if let (Some(ns), Some(c)) = (ns, c)
                    && let (Some(ns), Some(c)) = (ns.as_str_bytes(), c.as_str_bytes())
                {
                    fault_code_ns = Some(ns.to_vec());
                    fault_code = Some(c.to_vec());
                }
            }
            _ => {}
        }
        if given && fault_code.as_ref().is_none_or(Vec::is_empty) {
            let ce = Throwables::of(req)?;
            return Err(argument_error(
                req,
                ce.value_error,
                1,
                "is not a valid fault code",
            ));
        }
        let object = this.object();
        clear_fault(req, object)?;
        let name = name.filter(|n| !n.is_empty());
        let details =
            (!matches!(details.as_ref(), ValueRef::Null | ValueRef::Undef)).then_some(details);
        set_soap_fault(
            req,
            object,
            &Fault {
                code_ns: fault_code_ns.as_deref(),
                code: fault_code.as_deref(),
                string: Some(string.as_bytes()),
                actor: actor.as_ref().map(PhpString::as_bytes),
                detail: details,
                name: None,
                lang: b"en",
            },
        )?;
        // `ZVAL_STR_COPY(Z_FAULT_NAME_P(obj), name)`: the argument's
        // string, one more owner, never a copy of its bytes.
        if let Some(name) = name {
            let n = Owner::take(req, Value::string(name));
            set_fault_slot(req, object, FaultSlot::Name, n)?;
        }
        if !matches!(headerFault.as_ref(), ValueRef::Null | ValueRef::Undef) {
            let h = Owner::take(req, headerFault.deref());
            set_fault_slot(req, object, FaultSlot::HeaderFault, h)?;
        }
        Ok(())
    }

    /// `SoapFault::__toString(): string`: `SoapFault exception: [code]
    /// string in file:line`, then the trace.
    ///
    /// ```php
    /// $f = new SoapFault('Server', 'Oops');
    /// echo $f, "\n";
    /// function g() { return new SoapFault(['urn:x', 'C'], 'deep'); }
    /// echo g(), "\n";
    /// ```
    /// ```expect
    /// SoapFault exception: [Server] Oops in %s:2
    /// Stack trace:
    /// #0 {main}
    /// SoapFault exception: [C] deep in %s:4
    /// Stack trace:
    /// #0 %s(5): g()
    /// #1 {main}
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> R<Owner<PhpString>> {
        let object = this.object();
        let faultcode = read_prop(req, &object, b"faultcode")?;
        let faultstring = read_prop(req, &object, b"faultstring")?;
        let file = read_prop(req, &object, b"file")?;
        let line = read_prop(req, &object, b"line")?;
        let trace = call_method(req, &object, b"gettraceasstring", &[])?;
        // Each converted in php's order, those made given up where one
        // throws.
        let mut texts: Vec<Owner<PhpString>> = Vec::with_capacity(4);
        let mut line_n = 0;
        let mut failed = None;
        for (at, value) in [&faultcode, &faultstring, &file, &line, &*trace]
            .into_iter()
            .enumerate()
        {
            let step = if at == 3 {
                to_long(req, value).map(|n| line_n = n)
            } else {
                to_string(req, value).map(|s| texts.push(s))
            };
            if let Err(thrown) = step {
                failed = Some(thrown);
                break;
            }
        }
        trace.give_up(req);
        if let Some(thrown) = failed {
            for s in texts {
                s.give_up(req);
            }
            return Err(thrown);
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"SoapFault exception: [");
        out.extend_from_slice(texts[0].as_bytes());
        out.extend_from_slice(b"] ");
        out.extend_from_slice(texts[1].as_bytes());
        out.extend_from_slice(b" in ");
        out.extend_from_slice(texts[2].as_bytes());
        out.push(b':');
        out.extend_from_slice(line_n.to_string().as_bytes());
        out.extend_from_slice(b"\nStack trace:\n");
        if texts[3].is_empty() {
            out.extend_from_slice(b"#0 {main}\n");
        } else {
            out.extend_from_slice(texts[3].as_bytes());
        }
        for s in texts {
            s.give_up(req);
        }
        PhpString::new(req, &out)
    }
}

/// The arguments of `set_soap_fault`.
pub(crate) struct Fault<'a> {
    pub(crate) code_ns: Option<&'a [u8]>,
    pub(crate) code: Option<&'a [u8]>,
    pub(crate) string: Option<&'a [u8]>,
    pub(crate) actor: Option<&'a [u8]>,
    pub(crate) detail: Option<Value>,
    pub(crate) name: Option<&'a [u8]>,
    pub(crate) lang: &'a [u8],
}

/// `soap_fault_dtor_properties`: the eight slots back to their defaults.
fn clear_fault(req: &mut Request, object: PhpObject) -> R<()> {
    let empty = string(req, b"")?;
    set_fault_slot(req, object, FaultSlot::String, empty)?;
    for slot in [
        FaultSlot::Code,
        FaultSlot::CodeNs,
        FaultSlot::Actor,
        FaultSlot::Detail,
        FaultSlot::Name,
        FaultSlot::HeaderFault,
    ] {
        set_fault_slot(req, object, slot, Owner::null())?;
    }
    let empty = string(req, b"")?;
    set_fault_slot(req, object, FaultSlot::Lang, empty)
}

/// `set_soap_fault`: the string (and `Exception::$message`), the code
/// mapped by the SOAP version running, the actor, detail, name and
/// language written into `object`.
pub(crate) fn set_soap_fault(req: &mut Request, object: PhpObject, f: &Fault<'_>) -> R<()> {
    let text = init(req, f.string.unwrap_or_default())?;
    set_fault_slot(req, object, FaultSlot::String, text)?;
    // `zend_update_property_ex(..., Z_FAULT_STRING_P(obj))`: the string
    // just written, one more owner.
    let message = Owner::take(req, crate::shared::fault_slot(object, FaultSlot::String));
    php_ext::call::write_prop(req, &object, b"message", message)?;
    if let Some(code) = f.code {
        let version = req.state::<State>().soap_version;
        let (code, ns): (&[u8], Option<&[u8]>) = if let Some(ns) = f.code_ns {
            (code, Some(ns))
        } else if version == SOAP_1_1 {
            let ns = matches!(
                code,
                b"Client" | b"Server" | b"VersionMismatch" | b"MustUnderstand"
            )
            .then_some(model::SOAP_1_1_ENV_NAMESPACE);
            (code, ns)
        } else if version == SOAP_1_2 {
            match code {
                b"Client" => (b"Sender".as_slice(), Some(model::SOAP_1_2_ENV_NAMESPACE)),
                b"Server" => (b"Receiver".as_slice(), Some(model::SOAP_1_2_ENV_NAMESPACE)),
                b"VersionMismatch" | b"MustUnderstand" | b"DataEncodingUnknown" => {
                    (code, Some(model::SOAP_1_2_ENV_NAMESPACE))
                }
                _ => (code, None),
            }
        } else {
            (code, None)
        };
        if version == SOAP_1_1 || version == SOAP_1_2 || f.code_ns.is_some() {
            let c = init(req, code)?;
            set_fault_slot(req, object, FaultSlot::Code, c)?;
            if let Some(ns) = ns {
                let n = init(req, ns)?;
                set_fault_slot(req, object, FaultSlot::CodeNs, n)?;
            }
        }
    }
    if let Some(actor) = f.actor {
        let a = init(req, actor)?;
        set_fault_slot(req, object, FaultSlot::Actor, a)?;
    }
    if let Some(detail) = f.detail
        && !matches!(detail.as_ref(), ValueRef::Undef)
    {
        let d = Owner::take(req, detail.deref());
        set_fault_slot(req, object, FaultSlot::Detail, d)?;
    }
    if let Some(name) = f.name {
        let n = init(req, name)?;
        set_fault_slot(req, object, FaultSlot::Name, n)?;
    }
    let lang = lang(req, f.lang)?;
    set_fault_slot(req, object, FaultSlot::Lang, lang)
}

/// `ZVAL_STRING`, `zend_string_init`: a string of the request's own
/// whatever its length, the empty string and one byte among them, which
/// [`string`] hands out interned.
fn init(req: &mut Request, bytes: &[u8]) -> R<Owner<Value>> {
    Ok(PhpString::filled(req, bytes.len(), |b| b.copy_from_slice(bytes))?.into_value())
}

/// `ZVAL_STR_COPY(Z_FAULT_LANG_P(obj), lang)`: `en` is `soap_lang_en`,
/// which `PHP_MINIT` interns and which costs php's request nothing, here
/// the string the request keeps for it, made by its first fault until an
/// extension can intern at its engine start (#3873); the empty string is
/// `ZSTR_EMPTY_ALLOC()`; any other is the string `parse_packet_soap` made
/// of a response's `xml:lang`.
fn lang(req: &mut Request, bytes: &[u8]) -> R<Owner<Value>> {
    if bytes == b"en" || bytes.is_empty() {
        let kept = PhpString::interned(req, bytes)?;
        return Ok(Owner::take(req, Value::string(kept)));
    }
    init(req, bytes)
}

// ---- SoapVar

/// `SoapVar`: a value sent as the type given.
///
/// ```php
/// var_dump(new SoapVar('x', XSD_STRING, 'string', 'http://www.w3.org/2001/XMLSchema', 'node', ''));
/// var_dump((new SoapVar(null, null))->enc_type);
/// try { new SoapVar(1, 12345); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(SoapVar)#1 (6) {
///   ["enc_type"]=>
///   int(101)
///   ["enc_value"]=>
///   string(1) "x"
///   ["enc_stype"]=>
///   string(6) "string"
///   ["enc_ns"]=>
///   string(32) "http://www.w3.org/2001/XMLSchema"
///   ["enc_name"]=>
///   string(4) "node"
///   ["enc_namens"]=>
///   NULL
/// }
/// int(999998)
/// SoapVar::__construct(): Argument #2 ($encoding) is not a valid encoding
/// ```
#[php_class]
pub(crate) struct SoapVar;

#[php_class]
impl SoapVar {
    #[php(property)]
    const enc_type: i64;
    #[php(property)]
    const enc_value: Value = ConstValue::Null;
    #[php(property)]
    const enc_stype: Option<PhpString> = None;
    #[php(property)]
    const enc_ns: Option<PhpString> = None;
    #[php(property)]
    const enc_name: Option<PhpString> = None;
    #[php(property)]
    const enc_namens: Option<PhpString> = None;

    /// `SoapVar::__construct(mixed $data, ?int $encoding, ?string $typeName =
    /// null, ?string $typeNamespace = null, ?string $nodeName = null,
    /// ?string $nodeNamespace = null)`.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        data: Value,
        encoding: Option<i64>,
        #[php(default = null)] typeName: Option<PhpString>,
        #[php(default = null)] typeNamespace: Option<PhpString>,
        #[php(default = null)] nodeName: Option<PhpString>,
        #[php(default = null)] nodeNamespace: Option<PhpString>,
    ) -> R<()> {
        let object = this.object();
        match encoding {
            None => {
                object.set_slot(req, 0, Owner::int(model::ty::UNKNOWN_TYPE.into()))?;
            }
            Some(t) => {
                let known = i32::try_from(t)
                    .is_ok_and(|t| model::DEFAULT_ENCODING.iter().any(|b| b.type_ == t));
                if !known {
                    let ce = Throwables::of(req)?;
                    return Err(argument_error(
                        req,
                        ce.value_error,
                        2,
                        "is not a valid encoding",
                    ));
                }
                object.set_slot(req, 0, Owner::int(t))?;
            }
        }
        if !matches!(data.as_ref(), ValueRef::Null | ValueRef::Undef) {
            let d = Owner::take(req, data.deref());
            object.set_slot(req, 1, d)?;
        }
        for (slot, given) in [
            (2, typeName),
            (3, typeNamespace),
            (4, nodeName),
            (5, nodeNamespace),
        ] {
            if let Some(s) = given
                && !s.is_empty()
            {
                let s = Owner::take(req, Value::string(s));
                object.set_slot(req, slot, s)?;
            }
        }
        Ok(())
    }
}

// ---- Soap\Url and Soap\Sdl

/// `soap_url_object`: the parsed URL a client keeps between calls, the
/// one its kept connection goes to; none until a request made one.
#[derive(Default)]
pub(crate) struct UrlPayload {
    /// `uri`, php's `php_uri *`, its `port` defaulted for the scheme.
    pub(crate) uri: Option<Box<HeldUri>>,
}

/// `sizeof(php_uri)`: seven `zend_string *` and one `zend_long`.
const PHP_URI: usize = 64;

/// `_ZSTR_HEADER_SIZE`: what a `zend_string` takes before its bytes.
const ZSTR_HEADER: usize = 24;

/// A `php_uri` where php keeps it, on the request's heap: the struct
/// `php_uri_parse_to_struct` `ecalloc`s and the `zend_string` of each
/// component it read, at php's sizes, beside the components the client
/// reads; all of it freed with the object, as `soap_url_object_free`
/// calls `php_uri_struct_free`.
pub(crate) struct HeldUri {
    pub(crate) uri: php_ext_uri::UriStruct,
    blocks: Vec<Emalloc>,
}

impl HeldUri {
    /// `uri` laid on the heap; the `Err` is the memory limit's fatal,
    /// with what was laid down so far given back.
    pub(crate) fn new(req: &mut Request, uri: php_ext_uri::UriStruct) -> R<Box<HeldUri>> {
        let parts = [
            &uri.scheme,
            &uri.user,
            &uri.password,
            &uri.host,
            &uri.path,
            &uri.query,
            &uri.fragment,
        ];
        let sizes: Vec<usize> = std::iter::once(PHP_URI)
            .chain(
                parts
                    .into_iter()
                    .flatten()
                    .map(|s| ZSTR_HEADER + s.len() + 1),
            )
            .collect();
        let mut blocks = Vec::with_capacity(sizes.len());
        for size in sizes {
            match Emalloc::new(req, size) {
                Ok(block) => blocks.push(block),
                Err(thrown) => {
                    HeldUri { uri, blocks }.free(req);
                    return Err(thrown);
                }
            }
        }
        Ok(Box::new(HeldUri { uri, blocks }))
    }

    /// `php_uri_struct_free`.
    pub(crate) fn free(self, req: &mut Request) {
        for block in self.blocks {
            block.free(req);
        }
    }
}

/// `soap_sdl_object`: the description a client was made with.
#[derive(Default)]
pub(crate) struct SdlPayload {
    pub(crate) sdl: Option<std::sync::Arc<crate::model::Sdl>>,
}

/// What `Soap\Url`'s and `Soap\Sdl`'s handlers refuse: construction in
/// php's bare words, cloning and comparison. `Soap\Sdl` has nothing else
/// to override, php's typo leaving it the standard cast.
const OPAQUE: Refusals = Refusals::NONE.construction(None).cloning().comparison();

/// `soap_url_object_handlers`: [`OPAQUE`]'s refusals, and a cast to `int`
/// as the object's number.
pub(crate) struct UrlHandlers;

pub(crate) static URL_HANDLERS: UrlHandlers = UrlHandlers;

impl ObjectHandlers for UrlHandlers {
    fn refusals(&self) -> Refusals {
        OPAQUE
    }

    fn cast_object(
        &self,
        req: &mut Request,
        object: PhpObject,
        to: TypeTag,
    ) -> R<Option<Owner<Value>>> {
        if to == TypeTag::Int {
            let id = Value::int(i64::from(object.index()));
            return Ok(Some(Owner::take(req, id)));
        }
        Std.cast_object(req, object, to)
    }

    /// `soap_url_object_free`: the `php_uri` given back with the object.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let held = object
            .native_mut::<UrlPayload>(req)
            .and_then(|payload| payload.uri.take());
        if let Some(held) = held {
            held.free(req);
        }
        Std.free_obj(req, object);
    }
}

/// `Soap\Url`: final, opaque, not serializable, no dynamic property
/// (`@strict-properties`).
///
/// ```php
/// try { new Soap\Url; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { new Soap\Sdl; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $r = new ReflectionClass('Soap\Url');
/// var_dump($r->isFinal(), $r->isCloneable());
/// try { $r->newInstanceWithoutConstructor(); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot directly construct Soap\Url
/// Cannot directly construct Soap\Sdl
/// bool(true)
/// bool(false)
/// Class Soap\Url is an internal class marked as final that cannot be instantiated without invoking its constructor
/// ```
///
/// The one a request kept, read off a client whose server never answered:
///
/// ```php
/// $srv = stream_socket_server('tcp://127.0.0.1:0');
/// ini_set('default_socket_timeout', 0);
/// $c = new SoapClient(null, ['location' => 'http://' . stream_socket_get_name($srv, false) . '/', 'uri' => 'urn:t', 'connection_timeout' => 5, 'exceptions' => false]);
/// echo $c->f()->getMessage(), "\n";
/// $u = (new ReflectionProperty($c, 'httpurl'))->getValue($c);
/// echo get_class($u), "\n";
/// try { $u->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump(isset($u->x), $u->y);
/// try { clone $u; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($u); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Error Fetching http headers
/// Soap\Url
/// Cannot create dynamic property Soap\Url::$x
///
/// Warning: Undefined property: Soap\Url::$y in %s on line 9
/// bool(false)
/// NULL
/// Trying to clone an uncloneable object of class Soap\Url
/// Serialization of 'Soap\Url' is not allowed
/// ```
///
/// The one a call kept holds php's bytes, given back as the slot lets it
/// go: the object, its `php_uri` and a string per component the URL has,
/// here the scheme, host, path and query:
///
/// ```php
/// $srv = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; $c = stream_socket_accept($s, 10);'
///     . ' while (($l = fgets($c)) !== false && rtrim($l) !== "") {} fread($c, 4); fwrite($c, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"); fclose($c);';
/// $p = proc_open([PHP_BINARY, '-n', '-r', $srv], [1 => ['pipe', 'w']], $pipes);
/// $uri = 'http://' . trim(fgets($pipes[1])) . '/a/b?q';
/// $c = new SoapClient(null, ['location' => $uri, 'uri' => 'u']);
/// $c->__doRequest('<E/>', $uri, 'f', SOAP_1_1);
/// $url = new ReflectionProperty($c, 'httpurl');
/// $m = memory_get_usage(); $url->setValue($c, null); echo $m - memory_get_usage(), "\n";
/// proc_close($p);
/// ```
/// ```expect process
/// 248
/// ```
#[php_class(
    namespace(Soap),
    final,
    not_serializable,
    size = 64,
    payload = UrlPayload,
    handlers = URL_HANDLERS,
    dynamic_properties = forbidden
)]
pub(crate) struct Url;

#[php_class]
impl Url {}

/// `Soap\Sdl`: final, opaque, not serializable, no dynamic property
/// (`@strict-properties`).
///
/// ```php
/// $w = "<definitions xmlns='http://schemas.xmlsoap.org/wsdl/' xmlns:soap='http://schemas.xmlsoap.org/wsdl/soap/' targetNamespace='urn:t'><portType name='P'/><binding name='B' type='P'><soap:binding transport='http://schemas.xmlsoap.org/soap/http'/></binding><service name='S'><port name='Q' binding='B'><soap:address location='http://localhost/'/></port></service></definitions>";
/// $c = new SoapClient('data://text/plain,' . urlencode($w), ['cache_wsdl' => WSDL_CACHE_NONE]);
/// $s = (new ReflectionProperty($c, 'sdl'))->getValue($c);
/// echo get_class($s), "\n";
/// try { $s->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump(isset($s->x), $s->y);
/// try { clone $s; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($s); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// $r = new ReflectionClass($s);
/// var_dump($r->isFinal(), $r->isCloneable());
/// ```
/// ```expect
/// Soap\Sdl
/// Cannot create dynamic property Soap\Sdl::$x
///
/// Warning: Undefined property: Soap\Sdl::$y in %s on line 7
/// bool(false)
/// NULL
/// Trying to clone an uncloneable object of class Soap\Sdl
/// Serialization of 'Soap\Sdl' is not allowed
/// bool(true)
/// bool(false)
/// ```
#[php_class(
    namespace(Soap),
    final,
    not_serializable,
    payload = SdlPayload,
    handlers = OPAQUE,
    dynamic_properties = forbidden
)]
pub(crate) struct Sdl;

#[php_class]
impl Sdl {}
