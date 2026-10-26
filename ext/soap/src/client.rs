//! `SoapClient` (`soap.c`): its 36 private properties, the options its
//! constructor reads into them, and the call (`do_soap_call`,
//! `do_request`) that serialises the request, hands it to `__doRequest`
//! and parses the response.

use php_ext::abi::{ConstArray, ConstValue, Instance, Ref, This, Untyped};
use php_ext::attr::php_class;
use php_ext::call::{call_method, lookup_function};
use php_ext::coerce::{to_long, value_name};
use php_ext::raising::{Level, Thrown, argument_error, catch_fatal, docref, throw_value};
use php_ext::request::{Request, StreamCtx};
use php_ext::values::{
    Key as PhpKey, Owner, PhpArray, PhpObject, PhpString, Resource, Value, ValueRef, walk,
};
use php_ext_libxml::ffi::{Library, Node, Save};

use crate::classes::SoapFault;
use crate::message::{
    function_to_string, get_function, serialize_function_call, soap_binding, soap_create_typemap,
    type_to_string,
};
use crate::model::{BindingType, Sdl};
use crate::shared::{
    ClientSlot, add_soap_fault, add_soap_fault_ex, client_slot, client_throws, instance_of,
    is_null, new_sdl_object, sdl_of, set_client_slot,
};
use crate::throwables::Throwables;
use crate::{Error, Res, SOAP_1_1, SOAP_1_2, State, Typemap, raised};

type R<T> = Result<T, Thrown>;

/// `SoapClient`'s payload: the typemap its `typemap` option made, which
/// php keeps as pointers in the `typemap` property's array.
#[derive(Default)]
pub(crate) struct Client {
    pub(crate) typemap: Option<crate::Typemap>,
}

/// The typemap held, given back with the object.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

impl php_ext::abi::ObjectHandlers for Handlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let typemap = object
            .native_mut::<Client>(req)
            .and_then(|c| c.typemap.take());
        if let Some(t) = typemap {
            t.release(req);
        }
        php_ext::abi::Std.free_obj(req, object);
    }
}

/// `SOAP_CLIENT_BEGIN_CODE`'s saved state.
struct Saved {
    use_handler: bool,
    error_code: Option<&'static str>,
    error_object: Option<PhpObject>,
    soap_version: i64,
}

fn begin(req: &mut Request, this: PhpObject) -> Saved {
    let state = req.state::<State>();
    let saved = Saved {
        use_handler: state.use_soap_error_handler,
        error_code: state.error_code,
        error_object: state.error_object,
        soap_version: state.soap_version,
    };
    state.use_soap_error_handler = true;
    state.error_code = Some("Client");
    state.error_object = Some(this);
    saved
}

fn end(req: &mut Request, saved: &Saved) {
    let state = req.state::<State>();
    state.use_soap_error_handler = saved.use_handler;
    state.error_code = saved.error_code;
    state.error_object = saved.error_object;
    state.soap_version = saved.soap_version;
}

/// `SOAP_CLIENT_END_CODE`'s `zend_catch`: a fatal raised in the call,
/// which soap's handler made the `SoapFault` a throwing client throws
/// before the request bailed out ([`crate::server::soap_error_handler`]),
/// is caught here, and the call leaves with the fault; any other fatal
/// bails out on past the call. The fatal's own frames are left as php
/// leaves them, no `finally` served, and `error_get_last()` keeps what
/// it had:
///
/// ```php
/// class C extends SoapClient {
///     function __doRequest($r, $l, $a, $v, $o = false, $u = null): ?string {
///         try { echo $undefined; trigger_error("boom", E_USER_ERROR); } finally { echo "not served\n"; }
///     }
/// }
/// register_shutdown_function(function () { echo "shutdown\n"; });
/// $c = new C(null, ['location' => 'http://x/', 'uri' => 'u']);
/// try { $c->f(); } catch (SoapFault $e) { echo $e->faultcode, ": ", $e->getMessage(), "\n"; }
/// var_dump(error_get_last()["message"]);
/// $c = new C(null, ['location' => 'http://x/', 'uri' => 'u', 'exceptions' => false]);
/// $c->f();
/// echo "not reached\n";
/// ```
/// ```expect exit=255
/// Warning: Undefined variable $undefined in %s on line 4
///
/// Deprecated: Passing E_USER_ERROR to trigger_error() is deprecated since 8.4, throw an exception or call exit with a string message instead in %s on line 4
/// Client: boom
/// string(126) "Passing E_USER_ERROR to trigger_error() is deprecated since 8.4, throw an exception or call exit with a string message instead"
///
/// Warning: Undefined variable $undefined in %s on line 4
///
/// Deprecated: Passing E_USER_ERROR to trigger_error() is deprecated since 8.4, throw an exception or call exit with a string message instead in %s on line 4
///
/// Fatal error: boom in %s on line 4
/// shutdown
/// ```
///
/// What the callback held is not released as the fatal leaves it, so
/// its destructor is served at shutdown, after the program:
///
/// ```php
/// class D { function __construct(public $n) {} function __destruct() { echo "d ", $this->n, "\n"; } }
/// class C extends SoapClient {
///     function __doRequest($r, $l, $a, $v, $o = false, $u = null): ?string { $inner = new D("inner"); trigger_error("boom", E_USER_ERROR); }
/// }
/// $outer = new D("outer");
/// $c = new C(null, ['location' => 'http://x/', 'uri' => 'u']);
/// try { $c->f(); } catch (SoapFault $e) { echo get_class($e), " ", $e->faultstring, "\n"; }
/// unset($outer);
/// echo "end\n";
/// ```
/// ```expect exit=0
/// Deprecated: Passing E_USER_ERROR to trigger_error() is deprecated since 8.4, throw an exception or call exit with a string message instead in %s on line 4
/// SoapFault boom
/// d outer
/// end
/// d inner
/// ```
///
/// A response whose decoding meets the engine's fatal, in a typemap's
/// `from_xml`, is the same fault thrown, or the fatal where the client
/// does not throw:
///
/// ```php
/// $w = '<definitions xmlns="http://schemas.xmlsoap.org/wsdl/" xmlns:soap="http://schemas.xmlsoap.org/wsdl/soap/" xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:tns="urn:t" targetNamespace="urn:t"><message name="q"/><message name="r"><part name="r" type="xsd:string"/></message><portType name="P"><operation name="f"><input message="tns:q"/><output message="tns:r"/></operation></portType><binding name="B" type="tns:P"><soap:binding style="rpc" transport="http://schemas.xmlsoap.org/soap/http"/><operation name="f"><soap:operation soapAction="f"/><input><soap:body use="encoded" namespace="urn:t" encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"/></input><output><soap:body use="encoded" namespace="urn:t" encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"/></output></operation></binding><service name="S"><port name="p" binding="tns:B"><soap:address location="http://x/"/></port></service></definitions>';
/// class C extends SoapClient { function __doRequest($r, $l, $a, $v, $o = false, $u = null): ?string { return '<e:Envelope xmlns:e="http://schemas.xmlsoap.org/soap/envelope/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><e:Body><m:fResponse xmlns:m="urn:t"><r xsi:type="xsd:string">v</r></m:fResponse></e:Body></e:Envelope>'; } }
/// $tm = [['type_ns' => 'http://www.w3.org/2001/XMLSchema', 'type_name' => 'string', 'from_xml' => function ($x) { trigger_error("in from_xml", E_USER_ERROR); }]];
/// foreach ([true, false] as $ex) {
///     $c = new C('data://text/plain;base64,' . base64_encode($w), ['typemap' => $tm, 'exceptions' => $ex, 'cache_wsdl' => WSDL_CACHE_NONE]);
///     try { var_dump($c->f()); } catch (SoapFault $e) { echo $e->faultcode, ": ", $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect exit=255
/// Deprecated: Passing E_USER_ERROR to trigger_error() is deprecated since 8.4, throw an exception or call exit with a string message instead in %s on line 4
/// Client: in from_xml
///
/// Deprecated: Passing E_USER_ERROR to trigger_error() is deprecated since 8.4, throw an exception or call exit with a string message instead in %s on line 4
///
/// Fatal error: in from_xml in %s on line 4
/// ```
fn end_code<T>(req: &mut Request, r: Res<T>) -> R<T> {
    let thrown = match raised(req, r) {
        Err(thrown) => thrown,
        ok => return ok,
    };
    let ce = Throwables::of(req)?;
    Err(catch_fatal(req, thrown, ce.soap_fault).unwrap_or_else(|thrown| thrown))
}

/// `php_error_docref(NULL, E_ERROR, ...)`.
fn docref_error(req: &Request, message: &str) -> Error {
    crate::server::docref_error(req, message)
}

/// A string option.
fn str_option(options: PhpArray, name: &[u8]) -> Option<Value> {
    options
        .find(name)
        .map(|v| v.deref())
        .filter(|v| v.as_str_bytes().is_some())
}

/// `SoapClient`.
///
/// ```php
/// $c = new SoapClient(null, ['location' => 'http://localhost/x', 'uri' => 'urn:x', 'trace' => 1, 'exceptions' => 0, 'login' => 'me']);
/// $p = (array) $c;
/// var_dump(count($p), $p["\0SoapClient\0uri"], $p["\0SoapClient\0trace"], $p["\0SoapClient\0_exceptions"], $p["\0SoapClient\0_soap_version"], $p["\0SoapClient\0_login"]);
/// ```
/// ```expect
/// int(36)
/// string(5) "urn:x"
/// bool(true)
/// bool(false)
/// int(1)
/// string(2) "me"
/// ```
#[php_class(payload = Client, handlers = HANDLERS)]
pub(crate) struct SoapClient;

#[php_class]
impl SoapClient {
    #[php(property, private)]
    const uri: Option<PhpString> = None;
    #[php(property, private)]
    const style: Option<i64> = None;
    #[php(property, private)]
    const r#use: Option<i64> = None;
    #[php(property, private)]
    const location: Option<PhpString> = None;
    #[php(property, private)]
    const trace: bool = false;
    #[php(property, private)]
    const compression: Option<i64> = None;
    #[php(property, private)]
    const sdl: Option<Instance<crate::classes::Sdl>> = None;
    #[php(property, private)]
    const typemap: Option<PhpArray> = None;
    #[php(property, private)]
    const httpsocket: Untyped<Value> = ConstValue::Null;
    #[php(property, private)]
    const httpurl: Option<Instance<crate::classes::Url>> = None;
    #[php(property, private)]
    const _login: Option<PhpString> = None;
    #[php(property, private)]
    const _password: Option<PhpString> = None;
    #[php(property, private)]
    const _use_digest: bool = false;
    #[php(property, private)]
    const _digest: Option<PhpString> = None;
    #[php(property, private)]
    const _proxy_host: Option<PhpString> = None;
    #[php(property, private)]
    const _proxy_port: Option<i64> = None;
    #[php(property, private)]
    const _proxy_login: Option<PhpString> = None;
    #[php(property, private)]
    const _proxy_password: Option<PhpString> = None;
    #[php(property, private)]
    const _exceptions: bool = true;
    #[php(property, private)]
    const _encoding: Option<PhpString> = None;
    #[php(property, private)]
    const _classmap: Option<PhpArray> = None;
    #[php(property, private)]
    const _features: Option<i64> = None;
    #[php(property, private)]
    const _connection_timeout: i64 = 0;
    #[php(property, private)]
    const _stream_context: Untyped<Value> = ConstValue::Null;
    #[php(property, private)]
    const _user_agent: Option<PhpString> = None;
    #[php(property, private)]
    const _keep_alive: bool = true;
    #[php(property, private)]
    const _ssl_method: Option<i64> = None;
    #[php(property, private)]
    const _soap_version: i64;
    #[php(property, private)]
    const _use_proxy: Option<i64> = None;
    #[php(property, private)]
    const _cookies: PhpArray = ConstArray(&[]);
    #[php(property, private)]
    const __default_headers: Option<PhpArray> = None;
    #[php(property, private)]
    const __soap_fault: Option<Instance<SoapFault>> = None;
    #[php(property, private)]
    const __last_request: Option<PhpString> = None;
    #[php(property, private)]
    const __last_response: Option<PhpString> = None;
    #[php(property, private)]
    const __last_request_headers: Option<PhpString> = None;
    #[php(property, private)]
    const __last_response_headers: Option<PhpString> = None;

    /// `SoapClient::__construct(?string $wsdl, array $options = [])`: the
    /// options read into the properties, the description loaded where one
    /// is given; a refusal is a `SoapFault`.
    ///
    /// ```php
    /// foreach ([[], ['uri' => 'u'], ['location' => 'l'], ['uri' => 'u', 'location' => 'l', 'encoding' => 'nope'], ['uri' => 'u', 'location' => 'l', 'classmap' => [1]]] as $o) {
    ///     try { new SoapClient(null, $o); echo "ok\n"; } catch (SoapFault $f) { echo $f->faultcode, ': ', $f->getMessage(), "\n"; }
    /// }
    /// $c = new SoapClient(null, ['uri' => 'u', 'location' => 'l', 'ssl_method' => SOAP_SSL_METHOD_TLS]);
    /// ```
    /// ```expect
    /// Client: SoapClient::__construct(): 'uri' option is required in nonWSDL mode
    /// Client: SoapClient::__construct(): 'location' option is required in nonWSDL mode
    /// Client: SoapClient::__construct(): 'uri' option is required in nonWSDL mode
    /// Client: SoapClient::__construct(): Invalid 'encoding' option - 'nope'
    /// Client: SoapClient::__construct(): 'classmap' option must be an associative array
    ///
    /// Deprecated: SoapClient::__construct(): The "ssl_method" option is deprecated. Use "ssl" stream context options instead in %s on line %d
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        wsdl: Option<PhpString>,
        #[php(default = [])] options: PhpArray,
    ) -> R<()> {
        let object = this.object();
        let saved = begin(req, object);
        let given = req.frames().argument_count() > 1;
        let r = construct(req, object, wsdl, given.then_some(options));
        let r = end_code(req, r);
        end(req, &saved);
        r
    }

    /// `SoapClient::__call(string $name, array $args): mixed`: the call by
    /// name.
    ///
    /// ```php
    /// class C extends SoapClient { function __doRequest($r, $l, $a, $v, $o = false, $u = null): ?string { echo $r, "\n"; return null; } }
    /// $c = new C(null, ['location' => 'test://', 'uri' => 'urn:x', 'exceptions' => false]);
    /// $f = $c->hello(1, 'two');
    /// echo $f->faultstring, "\n";
    /// ```
    /// ```expect
    /// <?xml version="1.0" encoding="UTF-8"?>
    /// <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/" xmlns:ns1="urn:x" xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:SOAP-ENC="http://schemas.xmlsoap.org/soap/encoding/" SOAP-ENV:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><SOAP-ENV:Body><ns1:hello><param0 xsi:type="xsd:int">1</param0><param1 xsi:type="xsd:string">two</param1></ns1:hello></SOAP-ENV:Body></SOAP-ENV:Envelope>
    ///
    /// SoapClient::__doRequest() returned non string value
    /// ```
    ///
    /// Since 8.5.12 a multidimensional array's answer stops once the memory
    /// limit was met making its arrays, and the limit's fatal is the one
    /// beneath's, not a `SoapFault`:
    ///
    /// ```php
    /// ini_set('memory_limit', '16M');
    /// class C extends SoapClient {
    ///     public function __doRequest($request, $location, $action, $version, $oneWay = false, $uriParserClass = null): ?string {
    ///         return '<?xml version="1.0"?><E:Envelope xmlns:E="http://schemas.xmlsoap.org/soap/envelope/" xmlns:A="http://schemas.xmlsoap.org/soap/encoding/" xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:ns1="u"><E:Body><ns1:fResponse><p A:arrayType="xsd:string[' . implode(',', array_fill(0, 300, 1)) . ']">' . str_repeat('<i/>', 300) . '</p></ns1:fResponse></E:Body></E:Envelope>';
    ///     }
    /// }
    /// $c = new C(null, ['location' => 'l', 'uri' => 'u']);
    /// $c->f();
    /// echo "unreached\n";
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of 16777216 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 9
    /// ```
    #[php(tentative)]
    fn __call(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        args: PhpArray,
    ) -> R<Owner<Value>> {
        call_common(req, this.object(), name, args, None, None, None, None, None)
    }

    /// `SoapClient::__soapCall(string $name, array $args, ?array $options =
    /// null, $inputHeaders = null, &$outputHeaders = null): mixed`: the call
    /// with a location, an action and a URI of its own, headers sent and
    /// the response's headers given back.
    ///
    /// ```php
    /// class C extends SoapClient { function __doRequest($r, $l, $a, $v, $o = false, $u = null): ?string { echo $l, ' ', $a, "\n"; return '<e:Envelope xmlns:e="http://schemas.xmlsoap.org/soap/envelope/"><e:Header><h>x</h></e:Header><e:Body><r><v>5</v></r></e:Body></e:Envelope>'; } }
    /// $c = new C(null, ['location' => 'test://', 'uri' => 'urn:x']);
    /// var_dump($c->__soapCall('f', [], ['location' => 'test://other', 'soapaction' => 'act'], new SoapHeader('urn:h', 'h'), $out), $out);
    /// try { $c->__soapCall('f', [], null, 7); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// test://other act
    /// string(1) "5"
    /// array(1) {
    ///   ["h"]=>
    ///   string(1) "x"
    /// }
    /// SoapClient::__soapCall(): Argument #4 ($inputHeaders) must be of type SoapHeader|array|null, int given
    /// ```
    #[php(tentative)]
    fn __soapCall(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        args: PhpArray,
        #[php(default = null)] options: Option<PhpArray>,
        #[php(default = null)] inputHeaders: Untyped<Value>,
        #[php(default = null)] outputHeaders: Ref<Untyped<Value>>,
    ) -> R<Owner<Value>> {
        let object = this.object();
        let mut location = None;
        let mut soap_action = None;
        let mut uri = None;
        if let Some(o) = &options {
            location =
                str_option(*o, b"location").and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
            soap_action =
                str_option(*o, b"soapaction").and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
            uri = str_option(*o, b"uri").and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
        }
        let output = if outputHeaders.is_given() {
            let fresh = PhpArray::new(req)?;
            outputHeaders.set(req, Untyped(fresh.into_value()))?;
            Some(())
        } else {
            None
        };
        let headers = inputHeaders.0.deref();
        let soap_headers: Option<Vec<Value>> = match headers.as_ref() {
            ValueRef::Null | ValueRef::Undef => None,
            ValueRef::Array(a) => {
                // `soap_headers = Z_ARRVAL_P(headers)` (`Z/ext/soap/soap.c:2694`):
                // php keeps the table and reads its rows as views, as these
                // are read under the borrowed request.
                let list: Vec<Value> = walk::rows(req, a).map(|(_, v)| v.deref()).collect();
                if !verify_soap_headers(req, &list) {
                    return Err(docref_error(req, "Invalid SOAP header").raise(req));
                }
                Some(list)
            }
            ValueRef::Object(o) if instance_of(req, o, "SoapHeader") => Some(vec![headers]),
            _ => {
                let shown = value_name(req, &headers);
                let ce = Throwables::of(req)?;
                return Err(argument_error(
                    req,
                    ce.type_error,
                    4,
                    &[
                        b"must be of type SoapHeader|array|null, ".as_slice(),
                        &shown,
                        b" given",
                    ]
                    .concat(),
                ));
            }
        };
        let out_ref = output.map(|()| &outputHeaders);
        call_common(
            req,
            object,
            name,
            args,
            location.as_deref(),
            soap_action.as_deref(),
            uri.as_deref(),
            soap_headers,
            out_ref,
        )
    }

    /// `SoapClient::__getFunctions(): ?array`: the description's
    /// operations as signatures; `null` without one.
    ///
    /// ```php
    /// var_dump((new SoapClient(null, ['location' => 'l', 'uri' => 'u']))->__getFunctions());
    /// ```
    /// ```expect
    /// NULL
    /// ```
    #[php(tentative)]
    fn __getFunctions(req: &mut Request, this: This<Self>) -> R<Option<Owner<PhpArray>>> {
        let Some(sdl) = this_sdl(req, this.object()) else {
            return Ok(None);
        };
        let mut out = PhpArray::new(req)?;
        for f in sdl.functions.values() {
            let s = function_to_string(&sdl, f);
            let v = PhpString::new(req, &s)?;
            out.append(req, v.into_value())?;
        }
        Ok(Some(out))
    }

    /// `SoapClient::__getTypes(): ?array`: the description's types as
    /// declarations; `null` without one.
    ///
    /// ```php
    /// var_dump((new SoapClient(null, ['location' => 'l', 'uri' => 'u']))->__getTypes());
    /// ```
    /// ```expect
    /// NULL
    /// ```
    #[php(tentative)]
    fn __getTypes(req: &mut Request, this: This<Self>) -> R<Option<Owner<PhpArray>>> {
        let Some(sdl) = this_sdl(req, this.object()) else {
            return Ok(None);
        };
        let mut out = PhpArray::new(req)?;
        if let Some(types) = &sdl.types {
            for id in types.values() {
                let mut s = Vec::new();
                if let Some(t) = sdl.ty(*id) {
                    type_to_string(&sdl, t, &mut s, 0);
                }
                let v = PhpString::new(req, &s)?;
                out.append(req, v.into_value())?;
            }
        }
        Ok(Some(out))
    }

    /// `SoapClient::__getLastRequest(): ?string`.
    ///
    /// ```php
    /// var_dump((new SoapClient(null, ['location' => 'l', 'uri' => 'u']))->__getLastRequest());
    /// ```
    /// ```expect
    /// NULL
    /// ```
    #[php(tentative)]
    #[allow(clippy::unnecessary_wraps)] // the native's return shape
    fn __getLastRequest(this: This<Self>) -> R<Option<PhpString>> {
        Ok(slot_string(this.object(), ClientSlot::LastRequest))
    }

    /// `SoapClient::__getLastResponse(): ?string`.
    ///
    /// ```php
    /// var_dump((new SoapClient(null, ['location' => 'l', 'uri' => 'u']))->__getLastResponse());
    /// ```
    /// ```expect
    /// NULL
    /// ```
    #[php(tentative)]
    #[allow(clippy::unnecessary_wraps)] // the native's return shape
    fn __getLastResponse(this: This<Self>) -> R<Option<PhpString>> {
        Ok(slot_string(this.object(), ClientSlot::LastResponse))
    }

    /// `SoapClient::__getLastRequestHeaders(): ?string`.
    ///
    /// ```php
    /// var_dump((new SoapClient(null, ['location' => 'l', 'uri' => 'u']))->__getLastRequestHeaders());
    /// ```
    /// ```expect
    /// NULL
    /// ```
    #[php(tentative)]
    #[allow(clippy::unnecessary_wraps)] // the native's return shape
    fn __getLastRequestHeaders(this: This<Self>) -> R<Option<PhpString>> {
        Ok(slot_string(this.object(), ClientSlot::LastRequestHeaders))
    }

    /// `SoapClient::__getLastResponseHeaders(): ?string`.
    ///
    /// ```php
    /// var_dump((new SoapClient(null, ['location' => 'l', 'uri' => 'u']))->__getLastResponseHeaders());
    /// ```
    /// ```expect
    /// NULL
    /// ```
    #[php(tentative)]
    #[allow(clippy::unnecessary_wraps)] // the native's return shape
    fn __getLastResponseHeaders(this: This<Self>) -> R<Option<PhpString>> {
        Ok(slot_string(this.object(), ClientSlot::LastResponseHeaders))
    }

    /// `SoapClient::__doRequest(string $request, string $location, string
    /// $action, int $version, bool $oneWay = false, ?string $uriParserClass
    /// = null): ?string`: the request sent over HTTP, the response body
    /// answered, `null` with a fault kept where it failed.
    ///
    /// ```php
    /// $c = new SoapClient(null, ['location' => 'l', 'uri' => 'u', 'exceptions' => false]);
    /// var_dump($c->__doRequest('<x/>', 'test://', 'a', SOAP_1_1));
    /// echo get_class(((array) $c)["\0SoapClient\0__soap_fault"]), "\n";
    /// ```
    /// ```expect
    /// NULL
    /// SoapFault
    /// ```
    ///
    /// Since 8.5.12 a chunk's size is unsigned and its room asked for
    /// fallibly, so a size of `FFFFFFFF` past the memory limit is refused
    /// and the limit's fatal is raised as the call returns:
    ///
    /// ```php
    /// $srv = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; $c = stream_socket_accept($s, 10); $n = 0;'
    ///     . ' while (($l = fgets($c)) !== false && rtrim($l) !== "") if (preg_match("/^Content-Length: (\d+)/i", $l, $m)) $n = (int) $m[1];'
    ///     . ' if ($n) fread($c, $n); fwrite($c, $argv[1]); for ($i = 0; $i < (int) $argv[3]; $i++) if (@fwrite($c, $argv[2]) === false) break; fclose($c);';
    /// $p = proc_open([PHP_BINARY, '-n', '-r', $srv, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFF\r\n", "", "0"], [1 => ['pipe', 'w']], $pipes);
    /// $uri = 'http://' . trim(fgets($pipes[1]));
    /// $c = new SoapClient(null, ['location' => $uri, 'uri' => 'u']);
    /// $c->__doRequest('<E/>', $uri, 'f', SOAP_1_1);
    /// echo "unreached\n";
    /// ```
    /// ```expect process exit=255
    ///
    /// Fatal error: Allowed memory size of 134217728 bytes exhausted (tried to allocate 4294983680 bytes) in %s on line 8
    /// ```
    ///
    /// The room of a `Content-Length` is asked for fallibly since 8.5.12,
    /// and one of `INT_MAX` is no longer refused before it:
    ///
    /// ```php
    /// $srv = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; $c = stream_socket_accept($s, 10); $n = 0;'
    ///     . ' while (($l = fgets($c)) !== false && rtrim($l) !== "") if (preg_match("/^Content-Length: (\d+)/i", $l, $m)) $n = (int) $m[1];'
    ///     . ' if ($n) fread($c, $n); fwrite($c, $argv[1]); for ($i = 0; $i < (int) $argv[3]; $i++) if (@fwrite($c, $argv[2]) === false) break; fclose($c);';
    /// $p = proc_open([PHP_BINARY, '-n', '-r', $srv, "HTTP/1.1 200 OK\r\nContent-Length: 2147483647\r\n\r\n", "", "0"], [1 => ['pipe', 'w']], $pipes);
    /// $uri = 'http://' . trim(fgets($pipes[1]));
    /// $c = new SoapClient(null, ['location' => $uri, 'uri' => 'u']);
    /// $c->__doRequest('<E/>', $uri, 'f', SOAP_1_1);
    /// echo "unreached\n";
    /// ```
    /// ```expect process exit=255
    ///
    /// Fatal error: Allowed memory size of 134217728 bytes exhausted (tried to allocate 2147500032 bytes) in %s on line 8
    /// ```
    ///
    /// A body read to the connection's close grows 4096 bytes at a time,
    /// each growth after the first asked for fallibly since 8.5.12. The
    /// figure is a huge block's growth, in place or moved as the address
    /// space allows, so it varies between runs:
    ///
    /// ```php
    /// ini_set('memory_limit', '8M');
    /// $srv = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; $c = stream_socket_accept($s, 10); $n = 0;'
    ///     . ' while (($l = fgets($c)) !== false && rtrim($l) !== "") if (preg_match("/^Content-Length: (\d+)/i", $l, $m)) $n = (int) $m[1];'
    ///     . ' if ($n) fread($c, $n); fwrite($c, $argv[1]); for ($i = 0; $i < (int) $argv[3]; $i++) if (@fwrite($c, $argv[2]) === false) break; fclose($c);';
    /// $p = proc_open([PHP_BINARY, '-n', '-r', $srv, "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n", str_repeat("x", 32), "1048576"], [1 => ['pipe', 'w']], $pipes);
    /// $uri = 'http://' . trim(fgets($pipes[1]));
    /// $c = new SoapClient(null, ['location' => $uri, 'uri' => 'u']);
    /// $c->__doRequest('<E/>', $uri, 'f', SOAP_1_1);
    /// echo "unreached\n";
    /// ```
    /// ```expect process exit=255
    ///
    /// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate %d bytes) in %s on line 9
    /// ```
    ///
    /// Since 8.5.12 the head stops once the memory limit was met after a
    /// line; the figure varies as the body's does:
    ///
    /// ```php
    /// ini_set('memory_limit', '8M');
    /// $srv = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; $c = stream_socket_accept($s, 10); $n = 0;'
    ///     . ' while (($l = fgets($c)) !== false && rtrim($l) !== "") if (preg_match("/^Content-Length: (\d+)/i", $l, $m)) $n = (int) $m[1];'
    ///     . ' if ($n) fread($c, $n); fwrite($c, $argv[1]); for ($i = 0; $i < (int) $argv[3]; $i++) if (@fwrite($c, $argv[2]) === false) break; fclose($c);';
    /// $p = proc_open([PHP_BINARY, '-n', '-r', $srv, "HTTP/1.1 200 OK\r\n", "X-Pad: " . str_repeat("a", 4000) . "\r\n", "8192"], [1 => ['pipe', 'w']], $pipes);
    /// $uri = 'http://' . trim(fgets($pipes[1]));
    /// $c = new SoapClient(null, ['location' => $uri, 'uri' => 'u']);
    /// $c->__doRequest('<E/>', $uri, 'f', SOAP_1_1);
    /// echo "unreached\n";
    /// ```
    /// ```expect process exit=255
    ///
    /// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate %d bytes) in %s on line 9
    /// ```
    #[php(tentative)]
    fn __doRequest(
        req: &mut Request,
        this: This<Self>,
        request: PhpString,
        location: PhpString,
        action: PhpString,
        version: i64,
        #[php(default = false)] oneWay: bool,
        #[php(default = null)] uriParserClass: Option<PhpString>,
    ) -> R<Option<Owner<PhpString>>> {
        let object = this.object();
        let mut one_way = oneWay;
        if req.state::<State>().features & 2 != 0 {
            one_way = false;
        }
        let parser = uriParserClass.as_ref().map(PhpString::as_bytes);
        let action = Some(action.as_bytes());
        let answered = if one_way {
            crate::http::make_http_soap_request_one_way(
                req,
                object,
                request.as_bytes(),
                location.as_bytes(),
                action,
                version,
                parser,
            )
            .map(|ok| ok.then(Vec::new))
        } else {
            crate::http::make_http_soap_request(
                req,
                object,
                request.as_bytes(),
                location.as_bytes(),
                action,
                version,
                parser,
            )
        };
        // An `E_ERROR` reaches the soap handler only while a client or a
        // server runs; called on its own it is the plain fatal.
        let answered = raised(req, answered)?;
        match answered {
            Some(body) => Ok(Some(PhpString::new(req, &body)?)),
            None => Ok(None),
        }
    }

    /// `SoapClient::__setCookie(string $name, ?string $value = null):
    /// void`: a cookie sent with every call, removed without a value.
    ///
    /// ```php
    /// $c = new SoapClient(null, ['location' => 'l', 'uri' => 'u']);
    /// $c->__setCookie('a', 'b'); $c->__setCookie('c', 'd'); $c->__setCookie('c');
    /// var_dump($c->__getCookies());
    /// ```
    /// ```expect
    /// array(1) {
    ///   ["a"]=>
    ///   array(1) {
    ///     [0]=>
    ///     string(1) "b"
    ///   }
    /// }
    /// ```
    #[php(tentative)]
    fn __setCookie(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        #[php(default = null)] value: Option<PhpString>,
    ) -> R<()> {
        let object = this.object();
        let cookies = client_slot(object, ClientSlot::Cookies);
        let mut cookies = match cookies.as_ref() {
            ValueRef::Array(a) => {
                let mut a = Owner::take(req, a);
                a.separate(req)?;
                a
            }
            _ => PhpArray::new(req)?,
        };
        match value {
            None => {
                cookies.unset(req, &PhpKey::lookup(name))?;
            }
            Some(v) => {
                let mut cookie = PhpArray::new(req)?;
                let at = Owner::take(req, PhpKey::Int(0));
                cookie.set(req, at, Owner::take(req, Value::string(v)))?;
                let name = Owner::take(req, PhpKey::lookup(name));
                cookies.set(req, name, cookie.into_value())?;
            }
        }
        set_client_slot(req, object, ClientSlot::Cookies, cookies.into_value())
    }

    /// `SoapClient::__getCookies(): array`.
    ///
    /// ```php
    /// var_dump((new SoapClient(null, ['location' => 'l', 'uri' => 'u']))->__getCookies());
    /// ```
    /// ```expect
    /// array(0) {
    /// }
    /// ```
    #[php(tentative)]
    #[allow(clippy::unnecessary_wraps)] // the native's return shape
    fn __getCookies(req: &mut Request, this: This<Self>) -> R<Owner<PhpArray>> {
        match client_slot(this.object(), ClientSlot::Cookies).as_array() {
            Some(a) => Ok(Owner::take(req, a)),
            None => PhpArray::new(req),
        }
    }

    /// `SoapClient::__setSoapHeaders($headers = null): bool`: the headers
    /// every call sends, replaced.
    ///
    /// ```php
    /// $c = new SoapClient(null, ['location' => 'l', 'uri' => 'u']);
    /// var_dump($c->__setSoapHeaders(new SoapHeader('urn:h', 'a')), $c->__setSoapHeaders([new SoapHeader('urn:h', 'b')]), $c->__setSoapHeaders());
    /// try { $c->__setSoapHeaders(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// SoapClient::__setSoapHeaders(): Argument #1 ($headers) must be of type SoapHeader|array|null, int given
    /// ```
    #[php(tentative)]
    fn __setSoapHeaders(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] headers: Untyped<Value>,
    ) -> R<bool> {
        let object = this.object();
        let headers = headers.0.deref();
        match headers.as_ref() {
            ValueRef::Null | ValueRef::Undef => {
                set_client_slot(req, object, ClientSlot::DefaultHeaders, Owner::null())?;
            }
            ValueRef::Array(a) => {
                // `verify_soap_headers_array(Z_ARRVAL_P(headers))`
                // (`Z/ext/soap/soap.c:2906`) reads the rows only.
                let list: Vec<Value> = walk::rows(req, a).map(|(_, v)| v.deref()).collect();
                if !verify_soap_headers(req, &list) {
                    return Err(docref_error(req, "Invalid SOAP header").raise(req));
                }
                let headers = Owner::take(req, headers);
                set_client_slot(req, object, ClientSlot::DefaultHeaders, headers)?;
            }
            ValueRef::Object(o) if instance_of(req, o, "SoapHeader") => {
                let mut list = PhpArray::new(req)?;
                let headers = Owner::take(req, headers);
                list.append(req, headers)?;
                set_client_slot(req, object, ClientSlot::DefaultHeaders, list.into_value())?;
            }
            _ => {
                let shown = value_name(req, &headers);
                let ce = Throwables::of(req)?;
                return Err(argument_error(
                    req,
                    ce.type_error,
                    1,
                    &[
                        b"must be of type SoapHeader|array|null, ".as_slice(),
                        &shown,
                        b" given",
                    ]
                    .concat(),
                ));
            }
        }
        Ok(true)
    }

    /// `SoapClient::__setLocation(?string $location = null): ?string`: the
    /// endpoint replaced, the old one answered.
    ///
    /// ```php
    /// $c = new SoapClient(null, ['location' => 'l', 'uri' => 'u']);
    /// var_dump($c->__setLocation('m'), $c->__setLocation(''), $c->__setLocation());
    /// ```
    /// ```expect
    /// string(1) "l"
    /// string(1) "m"
    /// NULL
    /// ```
    #[php(tentative)]
    fn __setLocation(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] location: Option<PhpString>,
    ) -> R<Option<Owner<PhpString>>> {
        let object = this.object();
        let old = slot_string(object, ClientSlot::Location).map(|s| Owner::take(req, s));
        let new = match location {
            Some(l) if !l.is_empty() => Owner::take(req, Value::string(l)),
            _ => Owner::null(),
        };
        set_client_slot(req, object, ClientSlot::Location, new)?;
        Ok(old)
    }
}

/// A view of a string slot, none for `null`: returned as it stands, the
/// engine takes its own owner (`RETURN_COPY`).
fn slot_string(this: PhpObject, which: ClientSlot) -> Option<PhpString> {
    client_slot(this, which).as_php_string()
}

/// `FETCH_THIS_SDL`.
fn this_sdl(req: &Request, this: PhpObject) -> Option<std::sync::Arc<Sdl>> {
    sdl_of(req, &client_slot(this, ClientSlot::Sdl))
}

/// `verify_soap_headers_array`: every element a `SoapHeader`.
fn verify_soap_headers(req: &mut Request, list: &[Value]) -> bool {
    list.iter().all(|v| match v.deref().as_ref() {
        ValueRef::Object(o) => instance_of(req, o, "SoapHeader"),
        _ => false,
    })
}

/// A string slot written from an option.
fn set_str(req: &mut Request, this: PhpObject, which: ClientSlot, v: Value) -> R<()> {
    let v = Owner::take(req, v);
    set_client_slot(req, this, which, v)
}

/// The options `SoapClient::__construct` reads between making its
/// `StreamCtx` and keeping it in the client's slot: `location` to
/// `connection_timeout`, in php's order (`Z/ext/soap/soap.c:2061-2177`).
/// The `StreamCtx` is the caller's, which gives it up where this fails.
#[expect(
    clippy::too_many_lines,
    reason = "one run of one C function, kept whole"
)]
fn context_options(
    req: &mut Request,
    this: PhpObject,
    ht: PhpArray,
    nonwsdl: bool,
    context: StreamCtx,
    soap_version: &mut i64,
    typemap_ht: &mut Option<PhpArray>,
) -> Res<()> {
    match str_option(ht, b"location") {
        Some(v) => set_str(req, this, ClientSlot::Location, v)?,
        None if nonwsdl => {
            return Err(docref_error(
                req,
                "'location' option is required in nonWSDL mode",
            ));
        }
        None => {}
    }
    if let Some(ValueRef::Int(v)) = ht
        .find(b"soap_version")
        .map(|v| v.deref())
        .as_ref()
        .map(Value::as_ref)
        && (v == SOAP_1_1 || v == SOAP_1_2)
    {
        *soap_version = v;
    }
    if let Some(login) = str_option(ht, b"login") {
        set_str(req, this, ClientSlot::Login, login)?;
        if let Some(p) = str_option(ht, b"password") {
            set_str(req, this, ClientSlot::Password, p)?;
        }
        if let Some(ValueRef::Int(1)) = ht
            .find(b"authentication")
            .map(|v| v.deref())
            .as_ref()
            .map(Value::as_ref)
        {
            set_client_slot(req, this, ClientSlot::UseDigest, Owner::bool(true))?;
        }
    }
    if let Some(host) = str_option(ht, b"proxy_host") {
        set_str(req, this, ClientSlot::ProxyHost, host)?;
        if let Some(port) = ht.find(b"proxy_port").map(|v| v.deref()) {
            let n = to_long(req, &port)?;
            set_client_slot(req, this, ClientSlot::ProxyPort, Owner::int(n))?;
        }
        if let Some(l) = str_option(ht, b"proxy_login") {
            set_str(req, this, ClientSlot::ProxyLogin, l)?;
            if let Some(p) = str_option(ht, b"proxy_password") {
                set_str(req, this, ClientSlot::ProxyPassword, p)?;
            }
        }
    }
    if let Some(cert) = str_option(ht, b"local_cert") {
        context.set_option(req, b"ssl", b"local_cert", &cert)?;
        if let Some(pp) = str_option(ht, b"passphrase") {
            context.set_option(req, b"ssl", b"passphrase", &pp)?;
        }
    }
    if let Some(t) = ht.find(b"trace").map(|v| v.deref())
        && matches!(t.as_ref(), ValueRef::Bool(true) | ValueRef::Int(1))
    {
        set_client_slot(req, this, ClientSlot::Trace, Owner::bool(true))?;
    }
    if let Some(t) = ht.find(b"exceptions").map(|v| v.deref())
        && matches!(t.as_ref(), ValueRef::Bool(false) | ValueRef::Int(0))
    {
        set_client_slot(req, this, ClientSlot::Exceptions, Owner::bool(false))?;
    }
    if let Some(ValueRef::Int(c)) = ht
        .find(b"compression")
        .map(|v| v.deref())
        .as_ref()
        .map(Value::as_ref)
        && [
            &b"gzinflate"[..],
            b"gzdeflate",
            b"gzuncompress",
            b"gzcompress",
            b"gzencode",
        ]
        .iter()
        .all(|f| lookup_function(req, f).is_some())
    {
        set_client_slot(req, this, ClientSlot::Compression, Owner::int(c))?;
    }
    if let Some(enc) = str_option(ht, b"encoding") {
        let name = enc.as_str_bytes().unwrap_or_default().to_vec();
        if Library::known_encoding(&name) {
            set_str(req, this, ClientSlot::Encoding, enc)?;
        } else {
            let shown = String::from_utf8_lossy(&name).into_owned();
            return Err(docref_error(
                req,
                &format!("Invalid 'encoding' option - '{shown}'"),
            ));
        }
    }
    if let Some(cm) = ht.find(b"classmap").map(|v| v.deref())
        && let ValueRef::Array(a) = cm.as_ref()
    {
        if a.is_packed() {
            return Err(docref_error(
                req,
                "'classmap' option must be an associative array",
            ));
        }
        let cm = Owner::take(req, cm);
        set_client_slot(req, this, ClientSlot::Classmap, cm)?;
    }
    if let Some(tm) = ht.find(b"typemap").map(|v| v.deref())
        && let ValueRef::Array(a) = tm.as_ref()
        && !a.is_empty()
    {
        *typemap_ht = Some(a);
    }
    if let Some(ValueRef::Int(f)) = ht
        .find(b"features")
        .map(|v| v.deref())
        .as_ref()
        .map(Value::as_ref)
    {
        set_client_slot(req, this, ClientSlot::Features, Owner::int(f))?;
    }
    if let Some(t) = ht.find(b"connection_timeout").map(|v| v.deref()) {
        let n = to_long(req, &t)?;
        if n > 0 {
            set_client_slot(req, this, ClientSlot::ConnectionTimeout, Owner::int(n))?;
        }
    }
    Ok(())
}

/// The typemap's keys, each under `null`, in the client's `typemap`
/// slot, as `SoapClient::__construct` records the option it took.
fn show_typemap(req: &mut Request, this: PhpObject, typemap: &Typemap) -> Res<()> {
    let mut shown = PhpArray::new(req)?;
    for (k, _) in typemap.iter() {
        if let crate::table::Key::Str(name) = k
            && let Err(thrown) = shown.set_str(req, name, Owner::null())
        {
            shown.give_up(req);
            return Err(thrown.into());
        }
    }
    set_client_slot(req, this, ClientSlot::Typemap, shown.into_value())?;
    Ok(())
}

/// `SoapClient::__construct`'s body.
#[expect(clippy::too_many_lines, reason = "one C function, kept whole")]
fn construct(
    req: &mut Request,
    this: PhpObject,
    wsdl: Option<PhpString>,
    options: Option<PhpArray>,
) -> Res<()> {
    let mut cache_wsdl = crate::sdl::cache::default_cache_wsdl(req);
    let mut soap_version = SOAP_1_1;
    let mut typemap_ht: Option<PhpArray> = None;
    if let Some(ht) = &options {
        if wsdl.is_none() {
            match str_option(*ht, b"uri") {
                Some(v) => set_str(req, this, ClientSlot::Uri, v)?,
                None => {
                    return Err(docref_error(
                        req,
                        "'uri' option is required in nonWSDL mode",
                    ));
                }
            }
            if let Some(ValueRef::Int(s)) = ht
                .find(b"style")
                .map(|v| v.deref())
                .as_ref()
                .map(Value::as_ref)
                && (s == 1 || s == 2)
            {
                set_client_slot(req, this, ClientSlot::Style, Owner::int(s))?;
            }
            if let Some(ValueRef::Int(u)) = ht
                .find(b"use")
                .map(|v| v.deref())
                .as_ref()
                .map(Value::as_ref)
                && (u == 1 || u == 2)
            {
                set_client_slot(req, this, ClientSlot::Use, Owner::int(u))?;
            }
        }
        // `php_stream_context_from_zval(tmp, 1)` and one owner more, or
        // `php_stream_context_alloc()`.
        let context = if let Some(v) = ht.find(b"stream_context").map(|v| v.deref())
            && let Some(r) = v.as_resource()
        {
            let context = StreamCtx::fetch(req, r)?;
            Owner::take(req, context)
        } else {
            // The client's slot keeps it, as the fetched one's.
            StreamCtx::alloc(req)?
        };
        let configured = context_options(
            req,
            this,
            *ht,
            wsdl.is_none(),
            *context,
            &mut soap_version,
            &mut typemap_ht,
        );
        if let Err(err) = configured {
            context.give_up(req);
            return Err(err);
        }
        let context = context.into_resource().into_value();
        set_client_slot(req, this, ClientSlot::StreamCtx, context)?;
        if let Some(ValueRef::Int(c)) = ht
            .find(b"cache_wsdl")
            .map(|v| v.deref())
            .as_ref()
            .map(Value::as_ref)
        {
            cache_wsdl = c;
        }
        if let Some(ua) = str_option(*ht, b"user_agent") {
            set_str(req, this, ClientSlot::UserAgent, ua)?;
        }
        if let Some(t) = ht.find(b"keep_alive").map(|v| v.deref())
            && matches!(t.as_ref(), ValueRef::Bool(false) | ValueRef::Int(0))
        {
            set_client_slot(req, this, ClientSlot::KeepAlive, Owner::bool(false))?;
        }
        if let Some(ValueRef::Int(m)) = ht
            .find(b"ssl_method")
            .map(|v| v.deref())
            .as_ref()
            .map(Value::as_ref)
        {
            set_client_slot(req, this, ClientSlot::SslMethod, Owner::int(m))?;
            docref(
                req,
                Level::Deprecated,
                "The \"ssl_method\" option is deprecated. Use \"ssl\" stream context options instead",
            );
        }
    } else if wsdl.is_none() {
        return Err(docref_error(
            req,
            "'location' and 'uri' options are required in nonWSDL mode",
        ));
    }
    set_client_slot(req, this, ClientSlot::SoapVersion, Owner::int(soap_version))?;
    let mut sdl = None;
    if let Some(w) = &wsdl {
        let old = std::mem::replace(&mut req.state::<State>().soap_version, soap_version);
        let loaded = crate::sdl::cache::get_sdl(req, this, w.as_bytes(), cache_wsdl);
        req.state::<State>().soap_version = old;
        let loaded = loaded?;
        let object = new_sdl_object(req, loaded.clone())?;
        set_client_slot(req, this, ClientSlot::Sdl, object.into_value())?;
        sdl = Some(loaded);
    }
    if let Some(ht) = typemap_ht {
        match soap_create_typemap(req, sdl.as_deref(), ht) {
            Ok(Some(t)) => {
                if let Err(thrown) = show_typemap(req, this, &t) {
                    t.release(req);
                    return Err(thrown);
                }
                let old = match this.native_mut::<Client>(req) {
                    Some(c) => c.typemap.replace(t),
                    None => Some(t),
                };
                if let Some(old) = old {
                    old.release(req);
                }
            }
            Ok(None) => {}
            Err(t) => return Err(Error::Thrown(t)),
        }
    }
    Ok(())
}

/// `soap_client_call_common`: the default headers added to the call's,
/// the arguments in order, and the call made.
fn call_common(
    req: &mut Request,
    this: PhpObject,
    function: PhpString,
    args: PhpArray,
    location: Option<&[u8]>,
    soap_action: Option<&[u8]>,
    uri: Option<&[u8]>,
    soap_headers: Option<Vec<Value>>,
    output_headers: Option<&Ref<Untyped<Value>>>,
) -> R<Owner<Value>> {
    let mut headers = soap_headers;
    if let ValueRef::Array(defaults) = client_slot(this, ClientSlot::DefaultHeaders).as_ref() {
        // `ZEND_HASH_FOREACH_VAL(default_headers, tmp)`
        // (`Z/ext/soap/soap.c:2590`), read only, views of the rows.
        let defaults: Vec<Value> = walk::rows(req, defaults).map(|(_, v)| v.deref()).collect();
        match &mut headers {
            Some(list) => list.extend(
                defaults
                    .into_iter()
                    .filter(|v| v.deref().as_object().is_some()),
            ),
            None => headers = Some(defaults),
        }
    }
    // `ZEND_HASH_FOREACH_VAL(args, param)` with `ZVAL_COPY_VALUE`
    // (`Z/ext/soap/soap.c:2609`), read only, views of the rows the argument
    // holds.
    let real_args: Vec<Value> = walk::rows(req, args).map(|(_, v)| v.deref()).collect();
    do_soap_call(
        req,
        this,
        function.as_bytes(),
        &real_args,
        location,
        soap_action,
        uri,
        headers.as_deref(),
        output_headers,
    )
}

/// `do_request`: the request dumped, kept for `__getLastRequest()` under
/// `trace`, handed to `__doRequest()`, the response kept likewise; a
/// fault where `__doRequest()` answered no string.
fn do_request(
    req: &mut Request,
    this: PhpObject,
    request: Node,
    location: &[u8],
    action: Option<&[u8]>,
    version: i64,
    one_way: bool,
) -> Res<(bool, Option<Owner<Value>>)> {
    let (buf, _) = Save::doc(request, 0, false, None);
    let Some(buf) = buf else {
        add_soap_fault(
            req,
            this,
            b"HTTP",
            b"Error build soap request",
            None,
            None,
            b"en",
        )?;
        return Ok((false, None));
    };
    let p0 = PhpString::new(req, &buf)?.into_value();
    let p1 = PhpString::new(req, location)?.into_value();
    let p2 = PhpString::new(req, action.unwrap_or_default())?.into_value();
    let params = [*p0, *p1, *p2, Value::int(version), Value::bool(one_way)];
    let trace = matches!(
        client_slot(this, ClientSlot::Trace).as_ref(),
        ValueRef::Bool(true)
    );
    if trace {
        let kept = Owner::take(req, *p0);
        set_client_slot(req, this, ClientSlot::LastRequest, kept)?;
    }
    let response = call_method(req, &this, b"__dorequest", &params);
    for p in [p2, p1, p0] {
        p.give_up(req);
    }
    let response = response?;
    let mut ret = true;
    if response.as_str_bytes().is_none() {
        let fault_set = client_slot(this, ClientSlot::SoapFault)
            .as_object()
            .is_some();
        if !fault_set {
            add_soap_fault(
                req,
                this,
                b"Client",
                b"SoapClient::__doRequest() returned non string value",
                None,
                None,
                b"en",
            )?;
        }
        ret = false;
    } else if trace {
        let kept = Owner::take(req, *response);
        set_client_slot(req, this, ClientSlot::LastResponse, kept)?;
    }
    if ret
        && client_slot(this, ClientSlot::SoapFault)
            .as_object()
            .is_some()
    {
        ret = false;
    }
    Ok((ret, Some(response)))
}

/// `do_soap_call`.
fn do_soap_call(
    req: &mut Request,
    this: PhpObject,
    function: &[u8],
    args: &[Value],
    location: Option<&[u8]>,
    soap_action: Option<&[u8]>,
    call_uri: Option<&[u8]>,
    soap_headers: Option<&[Value]>,
    output_headers: Option<&Ref<Untyped<Value>>>,
) -> R<Owner<Value>> {
    let saved = begin(req, this);
    if matches!(
        client_slot(this, ClientSlot::Trace).as_ref(),
        ValueRef::Bool(true)
    ) {
        set_client_slot(req, this, ClientSlot::LastRequest, Owner::null())?;
        set_client_slot(req, this, ClientSlot::LastResponse, Owner::null())?;
    }
    let soap_version = match client_slot(this, ClientSlot::SoapVersion).as_ref() {
        ValueRef::Int(2) => SOAP_1_2,
        _ => SOAP_1_1,
    };
    let location: Option<Vec<u8>> = location.map(<[u8]>::to_vec).or_else(|| {
        client_slot(this, ClientSlot::Location)
            .as_str_bytes()
            .map(<[u8]>::to_vec)
    });
    let sdl = this_sdl(req, this);
    let typemap = this
        .native::<Client>(req)
        .and_then(|c| c.typemap.as_ref().map(Typemap::view));
    // `clear_soap_fault`.
    set_client_slot(req, this, ClientSlot::SoapFault, Owner::null())?;
    let encoding = client_slot(this, ClientSlot::Encoding)
        .as_str_bytes()
        .map(<[u8]>::to_vec);
    let class_map = {
        let v = client_slot(this, ClientSlot::Classmap);
        v.as_array().is_some().then_some(v)
    };
    let features = match client_slot(this, ClientSlot::Features).as_ref() {
        ValueRef::Int(f) => f,
        _ => 0,
    };
    let state = req.state::<State>();
    state.soap_version = soap_version;
    let old_sdl = std::mem::replace(&mut state.sdl, sdl.clone());
    let old_encoding = std::mem::replace(&mut state.encoding, encoding);
    let old_class_map = std::mem::replace(&mut state.class_map, class_map);
    let old_typemap = std::mem::replace(&mut state.typemap, typemap);
    let old_features = std::mem::replace(&mut state.features, features);
    let r = soap_call_body(
        req,
        this,
        sdl.as_deref(),
        function,
        args,
        location.as_deref(),
        soap_action,
        call_uri,
        soap_headers,
        output_headers,
        soap_version,
    );
    let r = end_code(req, r);
    let state = req.state::<State>();
    state.features = old_features;
    state.typemap = old_typemap;
    state.class_map = old_class_map;
    state.encoding = old_encoding;
    state.sdl = old_sdl;
    end(req, &saved);
    r
}

#[expect(clippy::too_many_lines, reason = "the body of one C function")]
fn soap_call_body(
    req: &mut Request,
    this: PhpObject,
    sdl: Option<&Sdl>,
    function: &[u8],
    args: &[Value],
    location: Option<&[u8]>,
    soap_action: Option<&[u8]>,
    call_uri: Option<&[u8]>,
    soap_headers: Option<&[Value]>,
    output_headers: Option<&Ref<Untyped<Value>>>,
    soap_version: i64,
) -> Res<Owner<Value>> {
    let ce = Throwables::of(req)?;
    let mut ret = false;
    let mut return_value: Option<Owner<Value>> = None;
    if let Some(sdl_ref) = sdl {
        if let Some(fn_) = get_function(Some(sdl_ref), function) {
            let binding = crate::message::binding_of(sdl_ref, fn_);
            let one_way = fn_.response_name.is_none()
                && fn_.response_parameters.is_none()
                && soap_headers.is_none();
            let loc: Vec<u8> = match location {
                Some(l) => l.to_vec(),
                None => binding.and_then(|b| b.location.clone()).unwrap_or_default(),
            };
            let request;
            let action: Option<Vec<u8>>;
            if binding.is_some_and(|b| b.binding_type == BindingType::Soap) {
                let fnb = soap_binding(Some(sdl_ref), Some(fn_));
                request = serialize_function_call(
                    req,
                    this,
                    Some(sdl_ref),
                    Some(fn_),
                    None,
                    fnb.and_then(|b| b.input.ns.as_deref()),
                    args,
                    soap_version,
                    soap_headers,
                )?;
                action = fnb.and_then(|b| b.soap_action.clone());
            } else {
                request = serialize_function_call(
                    req,
                    this,
                    Some(sdl_ref),
                    Some(fn_),
                    None,
                    sdl_ref.target_ns.as_deref(),
                    args,
                    soap_version,
                    soap_headers,
                )?;
                action = None;
            }
            let done = do_request(
                req,
                this,
                request,
                &loc,
                action.as_deref(),
                soap_version,
                one_way,
            );
            request.free_document();
            let (ok, response) = done?;
            ret = ok;
            if let Some(resp) = response {
                if ret && let Some(body) = resp.as_str_bytes() {
                    let body = body.to_vec();
                    let parsed = crate::packet::parse_packet_soap(
                        req,
                        this,
                        &body,
                        Some(fn_),
                        sdl,
                        output_headers,
                    );
                    resp.give_up(req);
                    let (r, v) = parsed?;
                    ret = r;
                    return_value = Some(v);
                } else {
                    resp.give_up(req);
                }
            }
        } else {
            let mut error = b"Function (\"".to_vec();
            error.extend_from_slice(function);
            error.extend_from_slice(b"\") is not a valid method for this service");
            add_soap_fault(req, this, b"Client", &error, None, None, b"en")?;
        }
    } else {
        let uri = client_slot(this, ClientSlot::Uri);
        if uri.as_str_bytes().is_none() {
            add_soap_fault(
                req,
                this,
                b"Client",
                b"Error finding \"uri\" property",
                None,
                None,
                b"en",
            )?;
        } else if location.is_none() {
            add_soap_fault(
                req,
                this,
                b"Client",
                b"Error could not find \"location\" property",
                None,
                None,
                b"en",
            )?;
        } else {
            let call_uri = call_uri
                .map(<[u8]>::to_vec)
                .or_else(|| uri.as_str_bytes().map(<[u8]>::to_vec))
                .unwrap_or_default();
            let request = serialize_function_call(
                req,
                this,
                None,
                None,
                Some(function),
                Some(&call_uri),
                args,
                soap_version,
                soap_headers,
            )?;
            let action = if let Some(a) = soap_action {
                a.to_vec()
            } else {
                let mut a = call_uri.clone();
                a.push(b'#');
                a.extend_from_slice(function);
                a
            };
            let done = do_request(
                req,
                this,
                request,
                location.unwrap_or_default(),
                Some(&action),
                soap_version,
                false,
            );
            request.free_document();
            let (ok, response) = done?;
            ret = ok;
            if let Some(resp) = response {
                if ret && let Some(body) = resp.as_str_bytes() {
                    let body = body.to_vec();
                    let parsed = crate::packet::parse_packet_soap(
                        req,
                        this,
                        &body,
                        None,
                        None,
                        output_headers,
                    );
                    resp.give_up(req);
                    let (r, v) = parsed?;
                    ret = r;
                    return_value = Some(v);
                } else {
                    resp.give_up(req);
                }
            }
        }
    }
    let fault = client_slot(this, ClientSlot::SoapFault);
    let return_value = if !ret || fault.as_object().is_some() {
        if let Some(v) = return_value {
            v.give_up(req);
        }
        if fault.as_object().is_some() {
            Owner::take(req, fault)
        } else {
            add_soap_fault_ex(req, this, b"Client", b"Unknown Error", None, None, b"en")?
                .into_value()
        }
    } else {
        match return_value {
            Some(v) => v,
            None => Owner::null(),
        }
    };
    if let ValueRef::Object(o) = return_value.as_ref()
        && o.instance_of(req, ce.soap_fault)
        && client_throws(this)
    {
        let thrown = throw_value(req, &return_value);
        return_value.give_up(req);
        return Err(Error::Thrown(thrown));
    }
    Ok(return_value)
}

#[allow(dead_code)]
fn _unused(_: Option<&Resource>) -> bool {
    is_null(&Value::null())
}
