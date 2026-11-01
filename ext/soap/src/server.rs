//! `SoapServer` (`soap.c`): the `soapService` a server object carries, its
//! methods, the request it reads (`deserialize_function_call`,
//! `deserialize_parameters`) and the faults it answers with
//! (`soap_server_fault`, `_soap_server_exception`, and
//! `soap_real_error_handler` for a server).

use std::sync::Arc;

use php_ext::abi::{Beneath, chain};
use php_ext::abi::{Instance, ObjectHandlers, Refusals, Std, This, Untyped, Variadic};
use php_ext::attr::php_class;
use php_ext::call::{call, lookup_class, lookup_function};
use php_ext::coerce::{callable_checked, value_name, with_string};
use php_ext::raising::{
    Level, Thrown, arg_value_error, argument_error, clear, docref, exit, leave_pending, pending,
    throw_error, throw_value,
};
use php_ext::request::{Ini, OutputOp, OwnedStream as _, Request, StartHandler, Streams as _};
use php_ext::values::{
    ClassId, Emalloc, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef, walk,
};
use php_ext_libxml::ffi::{Kind, NewDoc, Node, Save};

use crate::classes::{Fault as FaultArgs, SoapFault, SoapHeader};
use crate::encoding::{Cx, encode_finish, encode_reset_ns, master_to_zval};
use crate::message::{
    HeaderCall, get_doc_function, get_function, serialize_response_call, soap_binding,
    soap_create_typemap,
};
use crate::model::{
    Function, SOAP_1_1_ACTOR_NEXT, SOAP_1_1_ENC_NAMESPACE, SOAP_1_1_ENV_NAMESPACE,
    SOAP_1_2_ACTOR_NEXT, SOAP_1_2_ACTOR_UNLIMATERECEIVER, SOAP_1_2_ENC_NAMESPACE,
    SOAP_1_2_ENV_NAMESPACE, Sdl, Style,
};
use crate::shared::{add_soap_fault_ex, client_throws, instance_of, new_fault};
use crate::table::Table;
use crate::throwables::Throwables;
use crate::xml::{attr_content, get_attribute, get_node, node_is_equal};
use crate::{Error, Res, SOAP_1_1, SOAP_1_2, State, Typemap, TypemapView, raised};

type R<T> = Result<T, Thrown>;

/// `SOAP_FUNCTIONS`, `SOAP_CLASS`, `SOAP_OBJECT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceKind {
    Functions,
    Class,
    Object,
}

/// `SOAP_PERSISTENCE_SESSION`, `SOAP_PERSISTENCE_REQUEST`.
const PERSISTENCE_SESSION: i64 = 1;
const PERSISTENCE_REQUEST: i64 = 2;
/// `SOAP_FUNCTIONS_ALL`.
const FUNCTIONS_ALL: i64 = 999;

/// `soapService`.
pub(crate) struct Service {
    pub(crate) sdl: Option<Arc<Sdl>>,
    /// `soap_functions.ft`: the declared names by lowercased name.
    functions: Option<Table<Vec<u8>>>,
    /// `soap_functions.functions_all`.
    functions_all: bool,
    class: Option<ClassId>,
    /// `soap_class.argv`, one owner each.
    argv: Vec<Owner<Value>>,
    persistence: i64,
    /// `soap_object`, one owner.
    object: Option<Owner<Value>>,
    typemap: Option<Typemap>,
    version: i64,
    kind: ServiceKind,
    actor: Option<Vec<u8>>,
    uri: Option<Vec<u8>>,
    encoding: Option<Vec<u8>>,
    /// `class_map`, a copy with one owner.
    class_map: Option<Owner<Value>>,
    features: i64,
    send_errors: i64,
    /// `soap_headers_ptr` while `handle()` runs: what `addSoapHeader()`
    /// appended, one owner each.
    added_headers: Option<Vec<Owner<Value>>>,
    trace: bool,
    last_response_body: Option<Vec<u8>>,
}

impl Service {
    fn new() -> Service {
        Service {
            sdl: None,
            functions: None,
            functions_all: false,
            class: None,
            argv: Vec::new(),
            persistence: 0,
            object: None,
            typemap: None,
            version: SOAP_1_1,
            kind: ServiceKind::Functions,
            actor: None,
            uri: None,
            encoding: None,
            class_map: None,
            features: 0,
            send_errors: 1,
            added_headers: None,
            trace: false,
            last_response_body: None,
        }
    }

    /// `delete_service`: what it owns given back.
    fn release(self, req: &mut Request) {
        for v in self.argv {
            v.give_up(req);
        }
        if let Some(o) = self.object {
            o.give_up(req);
        }
        if let Some(c) = self.class_map {
            c.give_up(req);
        }
        if let Some(t) = self.typemap {
            t.release(req);
        }
        for v in self.added_headers.into_iter().flatten() {
            v.give_up(req);
        }
    }
}

/// `soap_server_object`: the `soapService`, none until the constructor ran.
#[derive(Default)]
pub(crate) struct Server {
    pub(crate) service: Option<Box<Service>>,
}

/// `soap_server_object_handlers`: the `soapService` freed with the object, and
/// no clone.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let service = object
            .native_mut::<Server>(req)
            .and_then(|s| s.service.take());
        if let Some(service) = service {
            service.release(req);
        }
        Std.free_obj(req, object);
    }
}

/// `SOAP_SERVER_BEGIN_CODE`'s saved state.
struct Saved {
    use_handler: bool,
    error_code: Option<&'static str>,
    error_object: Option<PhpObject>,
    soap_version: i64,
}

/// `SOAP_SERVER_BEGIN_CODE`.
fn begin(req: &mut Request, this: PhpObject) -> Saved {
    let state = req.state::<State>();
    let saved = Saved {
        use_handler: state.use_soap_error_handler,
        error_code: state.error_code,
        error_object: state.error_object,
        soap_version: state.soap_version,
    };
    state.use_soap_error_handler = true;
    state.error_code = Some("Server");
    state.error_object = Some(this);
    saved
}

/// `SOAP_SERVER_END_CODE`, which a fatal's `zend_bailout` never
/// reaches, since a server stands in no `zend_try` of its own: after a
/// fatal the handler stays on, with the server as its object, through
/// the shutdown functions, so a warning one raises is not shown.
///
/// ```php
/// function f($a) { echo "partial"; trigger_error("boom", E_USER_ERROR); }
/// $s = new SoapServer(null, ['uri' => 'http://test-uri/']);
/// $s->addFunction('f');
/// register_shutdown_function(function () { echo "\nsd\n"; echo $undefined; var_dump(use_soap_error_handler()); });
/// $s->handle('<?xml version="1.0"?><SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/" xmlns:ns1="http://test-uri/"><SOAP-ENV:Body><ns1:f><a>1</a></ns1:f></SOAP-ENV:Body></SOAP-ENV:Envelope>');
/// ```
/// ```expect exit=255
/// <?xml version="1.0" encoding="UTF-8"?>
/// <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/"><SOAP-ENV:Body><SOAP-ENV:Fault><faultcode>SOAP-ENV:Server</faultcode><faultstring>boom</faultstring><detail>partial</detail></SOAP-ENV:Fault></SOAP-ENV:Body></SOAP-ENV:Envelope>
///
/// sd
/// bool(true)
/// ```
fn end(req: &mut Request, saved: &Saved) {
    if req.diagnostics().bailing_out() {
        return;
    }
    let state = req.state::<State>();
    state.use_soap_error_handler = saved.use_handler;
    state.error_code = saved.error_code;
    state.error_object = saved.error_object;
    state.soap_version = saved.soap_version;
}

/// `php_error_docref(NULL, E_ERROR, ...)` under the soap handler: the
/// message as php words it, the running native before `(): `.
pub(crate) fn docref_error(req: &Request, message: &str) -> Error {
    Error::fatal(format!(
        "{}(): {message}",
        req.frames().active_function_name()
    ))
}

/// The fault `soap_real_error_handler` makes of a server's fatal: its
/// string the message, or `Internal Error` where the server does not
/// send errors, and its detail what the top buffer held, that buffer
/// discarded; one owner, the caller's.
fn fatal_fault(
    req: &mut Request,
    this: Option<PhpObject>,
    code: &str,
    message: &[u8],
) -> R<Owner<PhpObject>> {
    let send_errors = this
        .and_then(|o| o.native::<Server>(req))
        .and_then(|s| s.service.as_ref())
        .is_none_or(|s| s.send_errors != 0);
    let mut detail: Option<Owner<Value>> = None;
    let string: &[u8] = if send_errors {
        let level = match req.response_mut().buffer_op(&OutputOp::GetLevel) {
            Ok(v) => {
                let level = v.as_int().unwrap_or(0);
                v.give_up(req);
                level
            }
            Err(_) => 0,
        };
        if level > 0 {
            if let Ok(contents) = req.response_mut().buffer_op(&OutputOp::GetContents) {
                if contents.as_str_bytes().is_some_and(|s| !s.is_empty()) {
                    detail = Some(contents);
                } else {
                    contents.give_up(req);
                }
            }
            if let Ok(v) = req.response_mut().buffer_op(&OutputOp::EndClean) {
                v.give_up(req);
            }
        }
        message
    } else {
        b"Internal Error"
    };
    let made = new_fault(
        req,
        &FaultArgs {
            code_ns: None,
            code: Some(code.as_bytes()),
            string: Some(string),
            actor: None,
            detail: detail.as_ref().map(|d| **d),
            name: None,
            lang: b"en",
        },
    );
    if let Some(d) = detail {
        d.give_up(req);
    }
    made
}

/// `soap_error_handler` and `soap_real_error_handler`
/// (`Z/ext/soap/soap.c:1875-1977`), soap's link of `zend_error_cb`: the
/// one beneath serves every diagnostic until `use_soap_error_handler`
/// is on, as a client or a server turns it on for the span of its call.
/// Then, under a client, a fatal level is the `SoapFault` a throwing
/// client throws before the request bails out, for the call's
/// `zend_catch` to catch ([`crate::client`]); a client that does not
/// throw hands every diagnostic on, and a throwing one every one but
/// what the `WSDL` code raises, which is dropped unstored. Under a
/// server, or none, every diagnostic is handed on with `display_errors`
/// off around it, so it is stored and logged but not shown, and a fatal
/// level is first made a fault whose detail is what the top buffer held,
/// written as the response once the one beneath has served it. The
/// status line is taken off before the one beneath serves, and the line
/// and response code that stood are put back where it bails out, as
/// php's `zend_catch` puts them back; a diagnostic it does not bail out
/// of leaves the line gone.
///
/// A function a server serves that raises an engine fatal answers with
/// the fault, its detail what the function printed, and the request ends
/// in the fatal, which `error_get_last()` answers after; the warning
/// before it is not shown:
///
/// ```php
/// function f($a) { echo $undefined, "partial"; trigger_error("boom", E_USER_ERROR); }
/// $s = new SoapServer(null, ['uri' => 'http://test-uri/']);
/// $s->addFunction('f');
/// register_shutdown_function(function () { echo "\n", error_get_last()['message'], "\n"; });
/// $s->handle();
/// echo "not reached\n";
/// ```
/// ```request
/// --POST_RAW--
/// Content-Type: text/xml
/// <?xml version="1.0"?><SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/" xmlns:ns1="http://test-uri/"><SOAP-ENV:Body><ns1:f><a>1</a></ns1:f></SOAP-ENV:Body></SOAP-ENV:Envelope>
/// ```
/// ```expect exit=255
/// Status: 500 Internal Server Error
/// X-Powered-By: PHP/8.5.12
/// Content-Length: 295
/// Content-Type: text/xml; charset=utf-8
///
/// <?xml version="1.0" encoding="UTF-8"?>
/// <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/"><SOAP-ENV:Body><SOAP-ENV:Fault><faultcode>SOAP-ENV:Server</faultcode><faultstring>boom</faultstring><detail>partial</detail></SOAP-ENV:Fault></SOAP-ENV:Body></SOAP-ENV:Envelope>
///
/// boom
/// ```
///
/// So the `500` `php_error_cb` adds for a fatal with nothing displayed
/// is undone before the fault is written, and a client whose agent is
/// `Shockwave Flash`, which the fault writes no `500` for, is answered
/// `200`:
///
/// ```php
/// function f($a) { echo "partial"; trigger_error("boom", E_USER_ERROR); }
/// $s = new SoapServer(null, ['uri' => 'http://test-uri/']);
/// $s->addFunction('f');
/// $s->handle();
/// ```
/// ```request
/// --ENV--
/// HTTP_USER_AGENT=Shockwave Flash
/// --POST_RAW--
/// Content-Type: text/xml
/// <?xml version="1.0"?><SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/" xmlns:ns1="http://test-uri/"><SOAP-ENV:Body><ns1:f><a>1</a></ns1:f></SOAP-ENV:Body></SOAP-ENV:Envelope>
/// ```
/// ```expect exit=255
/// X-Powered-By: PHP/8.5.12
/// Content-Length: 295
/// Content-Type: text/xml; charset=utf-8
///
/// <?xml version="1.0" encoding="UTF-8"?>
/// <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/"><SOAP-ENV:Body><SOAP-ENV:Fault><faultcode>SOAP-ENV:Server</faultcode><faultstring>boom</faultstring><detail>partial</detail></SOAP-ENV:Fault></SOAP-ENV:Body></SOAP-ENV:Envelope>
/// ```
///
/// Turned on outside any call, the handler takes every diagnostic of the
/// request as a server's, with nothing running:
///
/// ```php
/// use_soap_error_handler(true);
/// echo $undefined, "next\n";
/// ob_start(); echo "buffered";
/// trigger_error("fatal", E_USER_ERROR);
/// echo "not reached\n";
/// ```
/// ```expect exit=255
/// next
/// <?xml version="1.0" encoding="UTF-8"?>
/// <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/"><SOAP-ENV:Body><SOAP-ENV:Fault><faultcode>SOAP-ENV:Server</faultcode><faultstring>fatal</faultstring><detail>buffered</detail></SOAP-ENV:Fault></SOAP-ENV:Body></SOAP-ENV:Envelope>
/// ```
pub(crate) fn soap_error_handler(req: &mut Request, beneath: Beneath<'_, chain::Error>) {
    let state = req.state::<State>();
    if !state.use_soap_error_handler {
        beneath.serve(req);
        return;
    }
    let (error_code, error_object) = (state.error_code, state.error_object);
    // `E_FATAL_ERRORS`.
    let fatal = matches!(
        beneath.level(),
        Level::Error
            | Level::CoreError
            | Level::CompileError
            | Level::UserError
            | Level::RecoverableError
            | Level::Parse
    );
    if let Some(client) = error_object.filter(|o| instance_of(req, *o, "SoapClient")) {
        let exceptions = client_throws(client);
        if req.memory().has_oom() {
            // Since 8.5.12 the memory limit's fatal is the one beneath's,
            // not a `SoapFault`.
            beneath.serve(req);
        } else if fatal && exceptions {
            // `add_soap_fault_ex_en` and `zend_throw_exception_object`,
            // then `zend_bailout()`, which the engine makes once this
            // returns; a fault the memory limit refuses has bailed out.
            let code = error_code.unwrap_or("Client");
            let made = add_soap_fault_ex(
                req,
                client,
                code.as_bytes(),
                beneath.message(),
                None,
                None,
                b"en",
            );
            let thrown = match made {
                Ok(fault) => {
                    let thrown = throw_value(req, &fault.as_value());
                    fault.give_up(req);
                    thrown
                }
                Err(thrown) => thrown,
            };
            leave_pending(req, thrown);
        } else if !exceptions || error_code != Some("WSDL") {
            // libxml's warnings while a throwing client parses its WSDL
            // are ignored.
            beneath.serve(req);
        }
        return;
    }
    let fault = if fatal {
        let code = error_code.unwrap_or("Server");
        match fatal_fault(req, error_object, code, beneath.message()) {
            Ok(fault) => Some(fault),
            Err(thrown) => {
                leave_pending(req, thrown);
                None
            }
        }
    } else {
        None
    };
    let display = req.diagnostics_mut().set_display_errors(0);
    // `SG(sapi_headers).http_status_line = NULL` before the `zend_try`,
    // and the line and code that stood put back in its `zend_catch`, so
    // the `500` `php_error_cb` adds as it bails out is undone; served and
    // not bailed out of, the line stays gone.
    let status = req.response_mut().take_http_status();
    let bails = beneath.bails();
    beneath.serve(req);
    if bails {
        req.response_mut().put_back_http_status(status);
    }
    req.diagnostics_mut().set_display_errors(display);
    if let Some(fault) = fault {
        // What the writing threw stays in `EG(exception)` as the
        // request bails out.
        if let Err(thrown) = soap_server_fault_ex(req, None, None, fault.as_value(), &[]) {
            leave_pending(req, thrown);
        }
        fault.give_up(req);
    }
}

/// `soap_server_fault_ex`: the fault serialised as the response, `500`
/// unless the agent is Shockwave Flash, its length and type, written.
fn soap_server_fault_ex(
    req: &mut Request,
    sdl: Option<&Sdl>,
    function: Option<&Function>,
    fault: Value,
    hdr: &[HeaderCall<'_>],
) -> R<()> {
    let version = req.state::<State>().soap_version;
    let doc = match serialize_response_call(req, sdl, function, None, None, fault, hdr, version) {
        Ok(doc) => doc,
        Err(Error::Thrown(t)) => return Err(t),
        Err(Error::Fatal { pending, .. }) => {
            if let Some(pending) = pending {
                leave_pending(req, pending);
            }
            None
        }
    };
    // The fault is freed as `d` drops, once it is dumped.
    let buf = doc
        .and_then(|d| Save::doc(d.node(), 0, false, None).0)
        .unwrap_or_default();
    let flash = server_var(req, b"HTTP_USER_AGENT").is_some_and(|a| a == b"Shockwave Flash");
    if !flash {
        req.response_mut()
            .header_add(b"HTTP/1.1 500 Internal Server Error", true, 0);
    }
    if output_compression(req) {
        req.response_mut().header_add(b"Connection: close", true, 0);
    } else {
        req.response_mut()
            .header_add(format!("Content-Length: {}", buf.len()).as_bytes(), true, 0);
    }
    if version == SOAP_1_2 {
        req.response_mut().header_add(
            b"Content-Type: application/soap+xml; charset=utf-8",
            true,
            0,
        );
    } else {
        req.response_mut()
            .header_add(b"Content-Type: text/xml; charset=utf-8", true, 0);
    }
    req.response_mut().write(&buf);
    Ok(())
}

/// `INI_INT("zlib.output_compression")`.
fn output_compression(req: &Request) -> bool {
    req.ini()
        .get(b"zlib.output_compression")
        .is_some_and(Ini::parse_bool)
}

/// `soap_server_fault`: a fault made of the arguments, written, and the
/// request ended; what leaves is the `exit`.
fn soap_server_fault(
    req: &mut Request,
    code: &[u8],
    string: &[u8],
    actor: Option<&[u8]>,
    details: Option<Value>,
    name: Option<&[u8]>,
    lang: &[u8],
) -> Error {
    let made = new_fault(
        req,
        &FaultArgs {
            code_ns: None,
            code: Some(code),
            string: Some(string),
            actor,
            detail: details,
            name,
            lang,
        },
    );
    let fault = match made {
        Ok(f) => f,
        Err(t) => return Error::Thrown(t),
    };
    let value = fault.as_value();
    let written = soap_server_fault_ex(req, None, None, value, &[]);
    fault.give_up(req);
    match written {
        Ok(()) => Error::Thrown(exit(req, &Value::null())),
        Err(t) => Error::Thrown(t),
    }
}

/// `soap_server_fault_en`.
fn server_fault_en(req: &mut Request, code: &str, string: &str) -> Error {
    soap_server_fault(
        req,
        code.as_bytes(),
        string.as_bytes(),
        None,
        None,
        None,
        b"en",
    )
}

/// `_soap_server_exception`: a `SoapFault` answered as the fault, an
/// `Error` as a `Server` fault with its message (or `Internal Error`),
/// both cleared; anything else handed on.
fn server_exception(
    req: &mut Request,
    this: PhpObject,
    sdl: Option<&Sdl>,
    function: Option<&Function>,
    thrown: Thrown,
) -> R<()> {
    let ce = Throwables::of(req)?;
    let Some(exception) = thrown.exception().copied() else {
        return Err(thrown);
    };
    if exception.instance_of(req, ce.soap_fault) {
        let value = exception.as_value();
        soap_server_fault_ex(req, sdl, function, value, &[])?;
        return clear(req, thrown);
    }
    if exception.instance_of(req, ce.error) {
        let send = this
            .native::<Server>(req)
            .and_then(|s| s.service.as_ref())
            .is_some_and(|s| s.send_errors != 0);
        let msg = if send {
            // `zend_read_property_ex(zend_ce_error, .., /* silent */ false)`.
            let m = exception.read_prop_in(
                req,
                php_ext::values::PropertyScope::Declaring,
                b"message",
            )?;
            let msg = with_string(req, &m, <[u8]>::to_vec);
            m.give_up(req);
            msg?
        } else {
            b"Internal Error".to_vec()
        };
        let fault = add_soap_fault_ex(req, this, b"Server", &msg, None, None, b"en")?;
        let value = fault.as_value();
        let written = soap_server_fault_ex(req, sdl, function, value, &[]);
        fault.give_up(req);
        written?;
        return clear(req, thrown);
    }
    Err(thrown)
}

/// The `soapService`, php's `Cannot fetch SoapServer object` where the
/// constructor never ran.
fn service_of(req: &mut Request, this: PhpObject) -> R<()> {
    if this
        .native::<Server>(req)
        .and_then(|s| s.service.as_ref())
        .is_none()
    {
        let ce = Throwables::of(req)?;
        return Err(throw_error(req, ce.error, "Cannot fetch SoapServer object"));
    }
    Ok(())
}

fn service_mut(req: &mut Request, this: PhpObject) -> Option<&mut Service> {
    this.native_mut::<Server>(req)
        .and_then(|s| s.service.as_deref_mut())
}

fn service_ref(req: &Request, this: PhpObject) -> Option<&Service> {
    this.native::<Server>(req)
        .and_then(|s| s.service.as_deref())
}

/// An option of the constructor's array.
fn option(options: Option<&PhpArray>, name: &[u8]) -> Option<Value> {
    options?.find(name).map(|v| v.deref())
}

/// `SoapServer`.
///
/// ```php
/// $s = new SoapServer(null, ['uri' => 'urn:x']);
/// var_dump($s, (new ReflectionClass('SoapServer'))->isCloneable());
/// try { clone $s; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(SoapServer)#1 (1) {
///   ["__soap_fault":"SoapServer":private]=>
///   NULL
/// }
/// bool(false)
/// Trying to clone an uncloneable object of class SoapServer
/// ```
#[php_class(payload = Server, handlers = HANDLERS)]
pub(crate) struct SoapServer;

#[php_class]
impl SoapServer {
    #[php(property, private)]
    const __soap_fault: Option<Instance<SoapFault>> = None;

    /// `SoapServer::__construct(?string $wsdl, array $options = [])`: the
    /// options read into a `soapService`, the description loaded where one is
    /// given; an option's refusal is a fault the server answers with.
    ///
    /// ```php
    /// $s = new SoapServer(null, ['uri' => 'urn:a', 'soap_version' => SOAP_1_2, 'actor' => 'urn:me', 'send_errors' => false]);
    /// var_dump($s->getFunctions());
    /// new SoapServer(null, ['uri' => 'urn:a', 'encoding' => 'no-such']);
    /// echo "not reached\n";
    /// ```
    /// ```expect exit=255
    /// array(0) {
    /// }
    /// <?xml version="1.0" encoding="UTF-8"?>
    /// <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/"><SOAP-ENV:Body><SOAP-ENV:Fault><faultcode>SOAP-ENV:Server</faultcode><faultstring>SoapServer::__construct(): Invalid 'encoding' option - 'no-such'</faultstring></SOAP-ENV:Fault></SOAP-ENV:Body></SOAP-ENV:Envelope>
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        wsdl: Option<PhpString>,
        #[php(default = [])] options: PhpArray,
    ) -> R<()> {
        let object = this.object();
        let saved = begin(req, object);
        let r = construct(req, object, wsdl, options);
        let r = raised(req, r);
        end(req, &saved);
        r
    }

    /// `SoapServer::fault(string $code, string $string, string $actor =
    /// "", mixed $details = null, string $name = "", string $lang = "")`:
    /// the fault written as the response and the request ended.
    ///
    /// ```php
    /// $s = new SoapServer(null, ['uri' => 'urn:a']);
    /// $s->fault('Client', 'You asked wrong', 'urn:actor', 'more');
    /// echo "not reached\n";
    /// ```
    /// ```expect
    /// <?xml version="1.0" encoding="UTF-8"?>
    /// <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/"><SOAP-ENV:Body><SOAP-ENV:Fault><faultcode>SOAP-ENV:Client</faultcode><faultstring>You asked wrong</faultstring><faultactor>urn:actor</faultactor><detail>more</detail></SOAP-ENV:Fault></SOAP-ENV:Body></SOAP-ENV:Envelope>
    /// ```
    #[php(tentative)]
    fn fault(
        req: &mut Request,
        this: This<Self>,
        code: PhpString,
        string: PhpString,
        #[php(default = "")] actor: PhpString,
        #[php(default = null)] details: Value,
        #[php(default = "")] name: PhpString,
        #[php(default = "")] lang: php_ext::abi::Path,
    ) -> R<()> {
        let object = this.object();
        let saved = begin(req, object);
        let r = (|| -> Res<()> {
            service_of(req, object)?;
            let encoding = service_ref(req, object).and_then(|s| s.encoding.clone());
            let old = std::mem::replace(&mut req.state::<State>().encoding, encoding);
            let details =
                (!matches!(details.as_ref(), ValueRef::Null | ValueRef::Undef)).then_some(details);
            let name = (!name.is_empty()).then(|| name.as_bytes().to_vec());
            let e = soap_server_fault(
                req,
                code.as_bytes(),
                string.as_bytes(),
                Some(actor.as_bytes()).filter(|a| !a.is_empty()),
                details,
                name.as_deref(),
                lang.0.as_bytes(),
            );
            req.state::<State>().encoding = old;
            Err(e)
        })();
        let r = raised(req, r);
        end(req, &saved);
        r
    }

    /// `SoapServer::addSoapHeader(SoapHeader $header): void`: a header
    /// block the response carries, only while `handle()` serves.
    ///
    /// ```php
    /// $s = new SoapServer(null, ['uri' => 'urn:a']);
    /// try { $s->addSoapHeader(new SoapHeader('urn:h', 'h')); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// SoapServer::addSoapHeader() may be called only during SOAP request processing
    /// ```
    #[php(tentative)]
    fn addSoapHeader(req: &mut Request, this: This<Self>, header: Instance<SoapHeader>) -> R<()> {
        let object = this.object();
        service_of(req, object)?;
        // `Z_ADDREF_P(fault)` for the list, given back with it.
        let value = Owner::take(req, header.object().as_value());
        let Some(service) = service_mut(req, object) else {
            value.give_up(req);
            return Ok(());
        };
        let Some(list) = service.added_headers.as_mut() else {
            value.give_up(req);
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
                "SoapServer::addSoapHeader() may be called only during SOAP request processing",
            ));
        };
        list.push(value);
        Ok(())
    }

    /// `SoapServer::setPersistence(int $mode): void`: session or request
    /// persistence of the class a server serves.
    ///
    /// ```php
    /// $s = new SoapServer(null, ['uri' => 'urn:a']);
    /// try { $s->setPersistence(SOAP_PERSISTENCE_REQUEST); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// class K {}
    /// $s->setClass('K');
    /// $s->setPersistence(SOAP_PERSISTENCE_REQUEST);
    /// try { $s->setPersistence(7); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// SoapServer::setPersistence(): Persistence cannot be set when the SOAP server is used in function mode
    /// SoapServer::setPersistence(): Argument #1 ($mode) must be either SOAP_PERSISTENCE_SESSION or SOAP_PERSISTENCE_REQUEST when the SOAP server is used in class mode
    /// ```
    #[php(tentative)]
    fn setPersistence(req: &mut Request, this: This<Self>, mode: i64) -> R<()> {
        let object = this.object();
        service_of(req, object)?;
        let kind = service_ref(req, object).map(|s| s.kind);
        if kind == Some(ServiceKind::Class) {
            if mode == PERSISTENCE_SESSION || mode == PERSISTENCE_REQUEST {
                if mode == PERSISTENCE_SESSION && req.extensions().named(b"session").is_none() {
                    let ce = Throwables::of(req)?;
                    return Err(throw_error(
                        req,
                        ce.error,
                        "SoapServer::setPersistence(): Session persistence cannot be enabled because the session module is not enabled",
                    ));
                }
                if let Some(s) = service_mut(req, object) {
                    s.persistence = mode;
                }
                Ok(())
            } else {
                Err(arg_value_error(
                    req,
                    1,
                    "be either SOAP_PERSISTENCE_SESSION or SOAP_PERSISTENCE_REQUEST when the SOAP server is used in class mode",
                ))
            }
        } else {
            let ce = Throwables::of(req)?;
            Err(throw_error(
                req,
                ce.error,
                "SoapServer::setPersistence(): Persistence cannot be set when the SOAP server is used in function mode",
            ))
        }
    }

    /// `SoapServer::setClass(string $class, mixed ...$args): void`: a
    /// class made per request, with the arguments its constructor takes.
    ///
    /// ```php
    /// $s = new SoapServer(null, ['uri' => 'urn:a']);
    /// class K { function a() {} protected function b() {} }
    /// $s->setClass('k', 1, 2);
    /// var_dump($s->getFunctions());
    /// try { $s->setClass('Nope'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// array(1) {
    ///   [0]=>
    ///   string(1) "a"
    /// }
    /// SoapServer::setClass(): Argument #1 ($class) must be a valid class name, Nope given
    /// ```
    #[php(tentative)]
    fn setClass(
        req: &mut Request,
        this: This<Self>,
        class: PhpString,
        args: Variadic<Value>,
    ) -> R<()> {
        let object = this.object();
        let Some(ce) = php_ext::call::autoload(req, class.as_bytes())? else {
            let shown = String::from_utf8_lossy(class.as_bytes()).into_owned();
            let ce = Throwables::of(req)?;
            return Err(argument_error(
                req,
                ce.type_error,
                1,
                &format!("must be a valid class name, {shown} given"),
            ));
        };
        service_of(req, object)?;
        // `ZVAL_COPY` of each argument into `soap_class.argv`.
        let args: Vec<Owner<Value>> = args.into_iter().map(|a| Owner::take(req, a)).collect();
        let old = match service_mut(req, object) {
            Some(s) => {
                s.kind = ServiceKind::Class;
                s.class = Some(ce);
                s.persistence = PERSISTENCE_REQUEST;
                std::mem::replace(&mut s.argv, args)
            }
            None => args,
        };
        for v in old {
            v.give_up(req);
        }
        Ok(())
    }

    /// `SoapServer::setObject(object $object): void`: one object serves
    /// every call.
    ///
    /// ```php
    /// class K { function a() {} private function b() {} }
    /// $s = new SoapServer(null, ['uri' => 'urn:a']);
    /// $s->setObject(new K);
    /// var_dump($s->getFunctions());
    /// ```
    /// ```expect
    /// array(1) {
    ///   [0]=>
    ///   string(1) "a"
    /// }
    /// ```
    #[php(tentative)]
    fn setObject(req: &mut Request, this: This<Self>, object: PhpObject) -> R<()> {
        let server = this.object();
        service_of(req, server)?;
        // `ZVAL_OBJ_COPY(&service->soap_object, obj)`.
        let value = Owner::take(req, object.as_value());
        let old = match service_mut(req, server) {
            Some(s) => {
                s.kind = ServiceKind::Object;
                s.object.replace(value)
            }
            None => Some(value),
        };
        if let Some(o) = old {
            o.give_up(req);
        }
        Ok(())
    }

    /// `SoapServer::getFunctions(): array`: the public methods of the
    /// class or object served, every function under
    /// `SOAP_FUNCTIONS_ALL`, or the functions added.
    ///
    /// ```php
    /// function f1() {} function F2() {}
    /// $s = new SoapServer(null, ['uri' => 'urn:a']);
    /// $s->addFunction('f1'); $s->addFunction(['f2', 'F1']);
    /// var_dump($s->getFunctions());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   string(2) "f1"
    ///   [1]=>
    ///   string(2) "F2"
    /// }
    /// ```
    #[php(tentative)]
    fn getFunctions(req: &mut Request, this: This<Self>) -> R<Owner<PhpArray>> {
        let object = this.object();
        service_of(req, object)?;
        let mut out = PhpArray::new(req)?;
        let Some(s) = service_ref(req, object) else {
            return Ok(out);
        };
        let names: Vec<Vec<u8>> = match s.kind {
            ServiceKind::Object => {
                let class = s
                    .object
                    .as_ref()
                    .and_then(|o| o.as_object())
                    .map(|o| o.class());
                class.map_or_else(Vec::new, |c| public_methods(req, c))
            }
            ServiceKind::Class => s.class.map_or_else(Vec::new, |c| public_methods(req, c)),
            ServiceKind::Functions if s.functions_all => all_functions(req)?,
            ServiceKind::Functions => s
                .functions
                .as_ref()
                .map(|ft| ft.values().cloned().collect())
                .unwrap_or_default(),
        };
        for n in names {
            let v = PhpString::new(req, &n)?;
            out.append(req, v.into_value())?;
        }
        Ok(out)
    }

    /// `SoapServer::addFunction($functions): void`: a function by name,
    /// an array of names, or `SOAP_FUNCTIONS_ALL`, deprecated.
    ///
    /// ```php
    /// $s = new SoapServer(null, ['uri' => 'urn:a']);
    /// foreach (['nope', ['strlen', 1], 1.5, 3] as $f) {
    ///     try { $s->addFunction($f); } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// }
    /// $s->addFunction(SOAP_FUNCTIONS_ALL);
    /// var_dump(in_array('strlen', $s->getFunctions()));
    /// ```
    /// ```expect
    /// TypeError: SoapServer::addFunction(): Argument #1 ($functions) must be a valid function name, function "nope" not found
    /// TypeError: SoapServer::addFunction(): Argument #1 ($functions) must contain only strings
    /// TypeError: SoapServer::addFunction(): Argument #1 ($functions) must be of type array|string|int, float given
    /// ValueError: SoapServer::addFunction(): Argument #1 ($functions) must be SOAP_FUNCTIONS_ALL when an integer is passed
    ///
    /// Deprecated: Constant SOAP_FUNCTIONS_ALL is deprecated since 8.4, as enabling all functions is a possible security concern in %s on line %d
    ///
    /// Deprecated: SoapServer::addFunction(): Enabling all functions via SOAP_FUNCTIONS_ALL is deprecated since 8.4, due to possible security concerns. If all PHP functions should be enabled, the flattened return value of get_defined_functions() can be used in %s on line %d
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn addFunction(req: &mut Request, this: This<Self>, functions: Untyped<Value>) -> R<()> {
        let object = this.object();
        service_of(req, object)?;
        let given = functions.0.deref();
        match given.as_ref() {
            ValueRef::Array(list) => {
                let kind = service_ref(req, object).map(|s| s.kind);
                if kind != Some(ServiceKind::Functions) {
                    return Ok(());
                }
                if let Some(s) = service_mut(req, object)
                    && s.functions.is_none()
                {
                    s.functions_all = false;
                    s.functions = Some(Table::new());
                }
                let names: Vec<Option<Vec<u8>>> = walk::rows(req, list)
                    .map(|(_, f)| f.deref().as_str_bytes().map(<[u8]>::to_vec))
                    .collect();
                for name in names {
                    let Some(name) = name else {
                        let ce = Throwables::of(req)?;
                        return Err(argument_error(
                            req,
                            ce.type_error,
                            1,
                            "must contain only strings",
                        ));
                    };
                    let Some(declared) = declared_function(req, &name) else {
                        let shown = String::from_utf8_lossy(&name).into_owned();
                        return Err(php_ext::raising::type_error(
                            req,
                            &format!("SoapServer::addFunction(): Function \"{shown}\" not found"),
                        ));
                    };
                    if let Some(ft) = service_mut(req, object).and_then(|s| s.functions.as_mut()) {
                        ft.update_str(&name.to_ascii_lowercase(), declared);
                    }
                }
                Ok(())
            }
            ValueRef::Str(name) => {
                let name = name.as_bytes().to_vec();
                let Some(declared) = declared_function(req, &name) else {
                    let shown = String::from_utf8_lossy(&name).into_owned();
                    let ce = Throwables::of(req)?;
                    return Err(argument_error(
                        req,
                        ce.type_error,
                        1,
                        &format!("must be a valid function name, function \"{shown}\" not found"),
                    ));
                };
                if let Some(s) = service_mut(req, object) {
                    if s.functions.is_none() {
                        s.functions_all = false;
                        s.functions = Some(Table::new());
                    }
                    if let Some(ft) = s.functions.as_mut() {
                        ft.update_str(&name.to_ascii_lowercase(), declared);
                    }
                }
                Ok(())
            }
            ValueRef::Int(n) => {
                if n == FUNCTIONS_ALL {
                    docref(
                        req,
                        Level::Deprecated,
                        "Enabling all functions via SOAP_FUNCTIONS_ALL is deprecated since 8.4, due to possible security concerns. If all PHP functions should be enabled, the flattened return value of get_defined_functions() can be used",
                    );
                    if let Some(s) = service_mut(req, object) {
                        s.functions = None;
                        s.functions_all = true;
                    }
                    Ok(())
                } else {
                    Err(arg_value_error(
                        req,
                        1,
                        "be SOAP_FUNCTIONS_ALL when an integer is passed",
                    ))
                }
            }
            _ => {
                let shown = value_name(req, &given);
                let ce = Throwables::of(req)?;
                Err(argument_error(
                    req,
                    ce.type_error,
                    1,
                    &[
                        b"must be of type array|string|int, ".as_slice(),
                        &shown,
                        b" given",
                    ]
                    .concat(),
                ))
            }
        }
    }

    /// `SoapServer::handle(?string $request = null): void`: the request
    /// read, the function called and its response written.
    ///
    /// ```php
    /// function add($a, $b) { return $a + $b; }
    /// $s = new SoapServer(null, ['uri' => 'urn:a']);
    /// $s->addFunction('add');
    /// $s->handle('<?xml version="1.0"?><e:Envelope xmlns:e="http://schemas.xmlsoap.org/soap/envelope/"><e:Body><m:add xmlns:m="urn:a"><a>1</a><b>2</b></m:add></e:Body></e:Envelope>');
    /// echo "\n";
    /// $s->handle('<bad');
    /// echo "not reached\n";
    /// ```
    /// ```expect
    /// <?xml version="1.0" encoding="UTF-8"?>
    /// <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/" xmlns:ns1="urn:a" xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:SOAP-ENC="http://schemas.xmlsoap.org/soap/encoding/" SOAP-ENV:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><SOAP-ENV:Body><ns1:addResponse><return xsi:type="xsd:int">3</return></ns1:addResponse></SOAP-ENV:Body></SOAP-ENV:Envelope>
    ///
    /// <?xml version="1.0" encoding="UTF-8"?>
    /// <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/"><SOAP-ENV:Body><SOAP-ENV:Fault><faultcode>SOAP-ENV:Client</faultcode><faultstring>Bad Request</faultstring></SOAP-ENV:Fault></SOAP-ENV:Body></SOAP-ENV:Envelope>
    /// ```
    ///
    /// Diagnostics raised meanwhile are shown nowhere, as soap's handler
    /// turns `PG(display_errors)` off around each past the ini table
    /// ([`soap_error_handler`]): once the request has changed an entry,
    /// the call costs only its response buffer's growth, no
    /// `display_errors` row and no string:
    ///
    /// ```php
    /// function add($a, $b) { return $a + $b; }
    /// ini_set('from', 'x');
    /// $s = new SoapServer(null, ['uri' => 'urn:a']);
    /// $s->addFunction('add');
    /// $r = '<?xml version="1.0"?><e:Envelope xmlns:e="http://schemas.xmlsoap.org/soap/envelope/"><e:Body><m:add xmlns:m="urn:a"><a>1</a><b>2</b></m:add></e:Body></e:Envelope>';
    /// ob_start();
    /// $m = memory_get_usage(); $s->handle($r); $d = memory_get_usage() - $m;
    /// ob_end_clean(); echo $d, "\n";
    /// ```
    /// ```expect
    /// 56
    /// ```
    ///
    /// Since 8.5.12 the arguments are read into a table asked for fallibly
    /// and stop once the memory limit was met, none handed on; the call's
    /// entry raises the limit's fatal, which the server answers as a fault:
    ///
    /// ```php
    /// ini_set('memory_limit', '16M');
    /// function f() { echo "unreached\n"; }
    /// $request = '<?xml version="1.0"?><E:Envelope xmlns:E="http://schemas.xmlsoap.org/soap/envelope/" xmlns:ns1="u"><E:Body><ns1:f>'
    ///     . str_repeat('<p><a/></p>', 100000) . '</ns1:f></E:Body></E:Envelope>';
    /// $server = new SoapServer(null, ['uri' => 'u']);
    /// $server->addFunction('f');
    /// $server->handle($request);
    /// ```
    /// ```expect exit=255
    /// <?xml version="1.0" encoding="UTF-8"?>
    /// <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/"><SOAP-ENV:Body><SOAP-ENV:Fault><faultcode>SOAP-ENV:Server</faultcode><faultstring>Allowed memory size of 16777216 bytes exhausted (tried to allocate 2097152 bytes)</faultstring></SOAP-ENV:Fault></SOAP-ENV:Body></SOAP-ENV:Envelope>
    /// ```
    ///
    /// The headers stop once the memory limit was met, since 8.5.12, each
    /// one's arguments the siblings that follow it:
    ///
    /// ```php
    /// ini_set('memory_limit', '16M');
    /// function f() { echo "unreached\n"; }
    /// $request = '<?xml version="1.0"?><E:Envelope xmlns:E="http://schemas.xmlsoap.org/soap/envelope/" xmlns:ns1="u"><E:Header>'
    ///     . str_repeat('<ns1:h/>', 5000) . '</E:Header><E:Body><ns1:f/></E:Body></E:Envelope>';
    /// $server = new SoapServer(null, ['uri' => 'u']);
    /// $server->addFunction('f');
    /// $server->handle($request);
    /// ```
    /// ```expect exit=255
    /// <?xml version="1.0" encoding="UTF-8"?>
    /// <SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/"><SOAP-ENV:Body><SOAP-ENV:Fault><faultcode>SOAP-ENV:Server</faultcode><faultstring>Allowed memory size of 16777216 bytes exhausted (tried to allocate 2097152 bytes)</faultstring></SOAP-ENV:Fault></SOAP-ENV:Body></SOAP-ENV:Envelope>
    /// ```
    #[php(tentative)]
    fn handle(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] request: Option<PhpString>,
    ) -> R<()> {
        let object = this.object();
        let saved = begin(req, object);
        let r = handle(req, object, request);
        let r = raised(req, r);
        end(req, &saved);
        r
    }

    /// `SoapServer::__getLastResponse(): ?string`: the last response
    /// `handle()` wrote, where the `trace` option is on.
    ///
    /// ```php
    /// function f() { return 1; }
    /// $s = new SoapServer(null, ['uri' => 'urn:a', 'trace' => true]);
    /// $s->addFunction('f');
    /// var_dump($s->__getLastResponse());
    /// ob_start(); $s->handle('<e:Envelope xmlns:e="http://schemas.xmlsoap.org/soap/envelope/"><e:Body><f/></e:Body></e:Envelope>'); ob_end_clean();
    /// var_dump(strlen($s->__getLastResponse()) > 0);
    /// ```
    /// ```expect
    /// NULL
    /// bool(true)
    /// ```
    fn __getLastResponse(req: &mut Request, this: This<Self>) -> R<Option<Owner<PhpString>>> {
        let object = this.object();
        service_of(req, object)?;
        match service_ref(req, object).and_then(|s| s.last_response_body.clone()) {
            Some(b) => Ok(Some(PhpString::new(req, &b)?)),
            None => Ok(None),
        }
    }
}

/// `f->common.function_name` of the function named, case-insensitively.
fn declared_function(req: &Request, name: &[u8]) -> Option<Vec<u8>> {
    let f = lookup_function(req, &name.to_ascii_lowercase())?;
    req.symbols().function_decl(f).map(|d| d.name.0.to_vec())
}

/// Every function of `EG(function_table)`, as `get_defined_functions()`
/// lists them, internal then user.
fn all_functions(req: &mut Request) -> R<Vec<Vec<u8>>> {
    let all = req.symbols_mut().defined_functions()?;
    let mut out = Vec::new();
    for (_, group) in walk::rows(req, *all) {
        if let ValueRef::Array(list) = group.deref().as_ref() {
            for (_, name) in walk::rows(req, list) {
                if let Some(n) = name.deref().as_str_bytes() {
                    out.push(n.to_vec());
                }
            }
        }
    }
    all.give_up(req);
    Ok(out)
}

/// The public methods of a class's function table: its own, then each
/// parent's it does not override.
fn public_methods(req: &mut Request, class: ClassId) -> Vec<Vec<u8>> {
    // `ZEND_HASH_FOREACH_PTR(&ce->function_table, f)` with
    // `ZEND_ACC_PUBLIC`, in the table's order.
    req.symbols()
        .method_rows(class)
        .filter_map(|row| req.symbols().row_method(row))
        .filter(|m| m.visibility == php_ext::decl::Visibility::Public)
        .map(|m| m.function.name.0.to_vec())
        .collect()
}

/// `SoapServer::__construct`'s body.
#[expect(clippy::too_many_lines, reason = "one C function, kept whole")]
fn construct(
    req: &mut Request,
    this: PhpObject,
    wsdl: Option<PhpString>,
    options: PhpArray,
) -> Res<()> {
    let mut service = Box::new(Service::new());
    let mut cache_wsdl = crate::sdl::cache::default_cache_wsdl(req);
    let mut version = SOAP_1_1;
    let mut typemap_ht: Option<PhpArray> = None;
    let opts = Some(&options);
    if let Some(tmp) = option(opts, b"soap_version") {
        match tmp.as_ref() {
            ValueRef::Int(v) if v == SOAP_1_1 || v == SOAP_1_2 => version = v,
            _ => {
                return Err(docref_error(
                    req,
                    "'soap_version' option must be SOAP_1_1 or SOAP_1_2",
                ));
            }
        }
    }
    match option(opts, b"uri").as_ref().and_then(Value::as_str_bytes) {
        Some(uri) => service.uri = Some(uri.to_vec()),
        None if wsdl.is_none() => {
            return Err(docref_error(
                req,
                "'uri' option is required in nonWSDL mode",
            ));
        }
        None => {}
    }
    if let Some(actor) = option(opts, b"actor")
        .as_ref()
        .and_then(Value::as_str_bytes)
    {
        service.actor = Some(actor.to_vec());
    }
    if let Some(enc) = option(opts, b"encoding")
        .as_ref()
        .and_then(Value::as_str_bytes)
    {
        if php_ext_libxml::ffi::Library::known_encoding(enc) {
            service.encoding = Some(enc.to_vec());
        } else {
            let shown = String::from_utf8_lossy(enc).into_owned();
            return Err(docref_error(
                req,
                &format!("Invalid 'encoding' option - '{shown}'"),
            ));
        }
    }
    if let Some(cm) = option(opts, b"classmap")
        && let ValueRef::Array(a) = cm.as_ref()
    {
        if a.is_packed() {
            return Err(docref_error(
                req,
                "'classmap' option must be an associative array",
            ));
        }
        let copy = a.copy(req)?;
        service.class_map = Some(copy.into_value());
    }
    if let Some(tm) = option(opts, b"typemap")
        && let ValueRef::Array(a) = tm.as_ref()
        && !a.is_empty()
    {
        typemap_ht = Some(a);
    }
    if let Some(ValueRef::Int(f)) = option(opts, b"features").as_ref().map(Value::as_ref) {
        service.features = f;
    }
    if let Some(ValueRef::Int(c)) = option(opts, b"cache_wsdl").as_ref().map(Value::as_ref) {
        cache_wsdl = c;
    }
    if let Some(se) = option(opts, b"send_errors") {
        match se.as_ref() {
            ValueRef::Bool(false) => service.send_errors = 0,
            ValueRef::Bool(true) => service.send_errors = 1,
            ValueRef::Int(n) => service.send_errors = n,
            _ => {}
        }
    }
    if let Some(t) = option(opts, b"trace")
        && matches!(t.as_ref(), ValueRef::Bool(true) | ValueRef::Int(1))
    {
        service.trace = true;
    }
    service.version = version;
    service.kind = ServiceKind::Functions;
    service.functions_all = false;
    service.functions = Some(Table::new());
    if let Some(w) = &wsdl {
        let sdl = match crate::sdl::cache::get_sdl(req, this, w.as_bytes(), cache_wsdl) {
            Ok(s) => s,
            Err(e) => {
                Box::new(*service).release(req);
                return Err(e);
            }
        };
        if service.uri.is_none() {
            service.uri = Some(
                sdl.target_ns
                    .clone()
                    .unwrap_or_else(|| b"http://unknown-uri/".to_vec()),
            );
        }
        service.sdl = Some(sdl);
    }
    if let Some(ht) = typemap_ht {
        match soap_create_typemap(req, service.sdl.as_deref(), ht) {
            Ok(t) => service.typemap = t,
            Err(t) => {
                service.release(req);
                return Err(Error::Thrown(t));
            }
        }
    }
    let old = this
        .native_mut::<Server>(req)
        .and_then(|s| s.service.replace(service));
    if let Some(old) = old {
        old.release(req);
    }
    Ok(())
}

/// The function a server calls, as `deserialize_function_call` found it.
struct Call<'a> {
    function: Option<&'a Function>,
    function_name: Vec<u8>,
    /// The decoded arguments, one owner each.
    params: Vec<Owner<Value>>,
    version: i64,
    headers: Vec<HeaderCall<'a>>,
    /// The `zval` tables `deserialize_parameters` laid on the request's
    /// heap for the arguments and the headers' ([`Tables`]).
    tables: Tables,
}

impl Call<'_> {
    fn release(self, req: &mut Request) {
        release_all(req, self.params);
        release_headers(req, self.headers);
        self.tables.free(req);
    }
}

/// What a call answered, and then, as php reads `EG(exception)` after
/// `call_user_function`, what it left pending: the memory limit's fatal
/// raised at the call's entry leaves php's unwind exit and no answer
/// (8.5.12).
fn called_pending(req: &mut Request, called: R<Owner<Value>>) -> R<Owner<Value>> {
    let value = called?;
    if let Err(thrown) = pending(req) {
        value.give_up(req);
        return Err(thrown);
    }
    Ok(value)
}

/// `sizeof(zval)`.
const ZVAL_SIZE: usize = 16;

/// The `zval` table `deserialize_parameters` asks for each list of
/// arguments, `safe_emalloc(num_of_params, sizeof(zval), 0)`, of php's
/// size on the request's heap beside the engine's own list, each held
/// until the call's arguments are let go.
#[derive(Default)]
struct Tables(Vec<Emalloc>);

impl Tables {
    /// One table for `count` arguments, asked for fallibly where
    /// `fallibly`; `false` where the memory limit refused it.
    fn ask(&mut self, req: &mut Request, count: usize, fallibly: bool) -> R<bool> {
        let len = count.saturating_mul(ZVAL_SIZE);
        let table = if fallibly {
            req.memory_mut().fallible(|req| Emalloc::new(req, len))?
        } else {
            Some(Emalloc::new(req, len)?)
        };
        let refused = table.is_none();
        self.0.extend(table);
        Ok(!refused)
    }

    fn free(self, req: &mut Request) {
        for table in self.0 {
            table.free(req);
        }
    }
}

/// `SoapServer::handle`'s body.
#[expect(clippy::too_many_lines, reason = "one C function, kept whole")]
fn handle(req: &mut Request, this: PhpObject, arg: Option<PhpString>) -> Res<()> {
    service_of(req, this)?;
    let (version, sdl, typemap, encoding, class_map, features, kind, actor, uri) = {
        let Some(s) = service_ref(req, this) else {
            return Ok(());
        };
        (
            s.version,
            s.sdl.clone(),
            s.typemap.as_ref().map(Typemap::view),
            s.encoding.clone(),
            s.class_map.as_deref().copied(),
            s.features,
            s.kind,
            s.actor.clone(),
            s.uri.clone(),
        )
    };
    req.state::<State>().soap_version = version;
    if let Some(a) = &arg
        && a.len() > i32::MAX as usize
    {
        return Err(server_fault_en(req, "Server", "Input string is too long"));
    }
    let query = server_var(req, b"QUERY_STRING");
    if req.variables().request_method() == Some(b"GET")
        && query
            .as_deref()
            .is_some_and(|q| q.eq_ignore_ascii_case(b"wsdl"))
    {
        if let Some(sdl) = &sdl {
            req.response_mut()
                .header_add(b"Content-Type: text/xml; charset=utf-8", true, 0);
            let source = sdl.source.clone().unwrap_or_default();
            match req.open_stream(&source, b"rb", false)? {
                Some(stream) => {
                    let mut buf = vec![0u8; 8192];
                    loop {
                        let n = stream.read_into(req, &mut buf).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        req.response_mut().write(&buf[..n]);
                    }
                    stream.free(req);
                }
                None => return Err(server_fault_en(req, "Server", "Couldn't find WSDL")),
            }
            return Ok(());
        }
        return Err(server_fault_en(
            req,
            "Server",
            "WSDL generation is not supported yet",
        ));
    }
    let started = req.response_mut().buffer_op(&OutputOp::Start {
        handler: StartHandler::Default,
        chunk_size: 0,
        flags: 0x70,
    });
    match started {
        Ok(v) => v.give_up(req),
        Err(_) => return Err(docref_error(req, "ob_start failed")),
    }
    let mut soap_action: Option<Vec<u8>> = None;
    let doc_request = match &arg {
        None => {
            if req.variables().request_method().is_none() {
                return Ok(());
            }
            let mut compressed = false;
            if let Some(enc) = server_var(req, b"HTTP_CONTENT_ENCODING") {
                if matches!(enc.as_slice(), b"gzip" | b"x-gzip" | b"deflate") {
                    compressed = true;
                } else {
                    let shown = String::from_utf8_lossy(&enc).into_owned();
                    docref(
                        req,
                        Level::Warning,
                        &format!("Request is compressed with unknown compression '{shown}'"),
                    );
                    return Ok(());
                }
            }
            if let Some(a) = server_var(req, b"HTTP_SOAPACTION").filter(|a| !a.is_empty()) {
                soap_action = Some(a);
            }
            if compressed {
                // php appends `zlib.inflate` with `window` 0x2f, any
                // header, to the body's read filters: the body read and
                // inflated as `zlib_decode` does, then parsed.
                let Some(body) = inflated_input(req)? else {
                    docref(req, Level::Warning, "Can't uncompress compressed request");
                    return Ok(());
                };
                crate::xml::parse_memory(req, &body)?
            } else {
                crate::xml::parse_file(req, b"php://input")?
            }
        }
        Some(a) => crate::xml::parse_memory(req, a.as_bytes())?,
    };
    let Some(doc_request) = doc_request else {
        return Err(server_fault_en(req, "Client", "Bad Request"));
    };
    if doc_request.node().int_subset().is_some() {
        if let Some(env) = get_node(doc_request.node().children(), b"Envelope", None)
            && let Some(ns) = env.ns()
        {
            let h = crate::xml::href(ns);
            if h == SOAP_1_1_ENV_NAMESPACE {
                req.state::<State>().soap_version = SOAP_1_1;
            } else if h == SOAP_1_2_ENV_NAMESPACE {
                req.state::<State>().soap_version = SOAP_1_2;
            }
        }
        drop(doc_request);
        return Err(server_fault_en(
            req,
            "Server",
            "DTD are not supported by SOAP",
        ));
    }
    let old = swap_globals(req, sdl.clone(), encoding, class_map, typemap, features);
    let sdl_ref = sdl.as_deref();
    let r = serve(
        req,
        this,
        sdl_ref,
        doc_request,
        actor.as_deref(),
        soap_action.as_deref(),
        kind,
        uri.as_deref(),
    );
    // php's handler meets an `E_ERROR` where it is raised, under the
    // version the request named, before `handle` puts the globals back.
    let r = raised(req, r).map_err(Error::Thrown);
    restore_globals(req, old);
    r
}

/// `php://input` read whole and inflated by `zlib_decode`; none where
/// zlib is not loaded.
fn inflated_input(req: &mut Request) -> Res<Option<Vec<u8>>> {
    let Some(stream) = req.open_stream(b"php://input", b"rb", false)? else {
        return Ok(Some(Vec::new()));
    };
    let mut body = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = stream.read_into(req, &mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    stream.free(req);
    if lookup_function(req, b"zlib_decode").is_none() {
        return Ok(None);
    }
    let name = PhpString::new(req, b"zlib_decode")?.into_value();
    let callable = php_ext::coerce::to_callable(req, &name);
    name.give_up(req);
    let arg = PhpString::new(req, &body)?.into_value();
    let out = match callable {
        Ok(callable) => call(req, &callable, &[*arg]),
        Err(thrown) => Err(thrown),
    };
    arg.give_up(req);
    let out = out?;
    let bytes = out.as_str_bytes().map(<[u8]>::to_vec);
    out.give_up(req);
    Ok(Some(bytes.unwrap_or_default()))
}

/// The `SOAP_GLOBAL` state a call runs under, put back after.
struct Globals {
    soap_version: i64,
    encoding: Option<Vec<u8>>,
    sdl: Option<Arc<Sdl>>,
    class_map: Option<Value>,
    typemap: Option<TypemapView>,
    features: i64,
}

fn swap_globals(
    req: &mut Request,
    sdl: Option<Arc<Sdl>>,
    encoding: Option<Vec<u8>>,
    class_map: Option<Value>,
    typemap: Option<TypemapView>,
    features: i64,
) -> Globals {
    let state = req.state::<State>();
    Globals {
        soap_version: state.soap_version,
        encoding: std::mem::replace(&mut state.encoding, encoding),
        sdl: std::mem::replace(&mut state.sdl, sdl),
        class_map: std::mem::replace(&mut state.class_map, class_map),
        typemap: std::mem::replace(&mut state.typemap, typemap),
        features: std::mem::replace(&mut state.features, features),
    }
}

fn restore_globals(req: &mut Request, g: Globals) {
    let state = req.state::<State>();
    state.soap_version = g.soap_version;
    state.encoding = g.encoding;
    state.sdl = g.sdl;
    state.class_map = g.class_map;
    state.typemap = g.typemap;
    state.features = g.features;
}

/// A `$_SERVER` entry that is a string.
fn server_var(req: &mut Request, name: &[u8]) -> Option<Vec<u8>> {
    let server = req.variables_mut().superglobal(b"_SERVER")?;
    server
        .find(name)?
        .deref()
        .as_str_bytes()
        .map(<[u8]>::to_vec)
}

/// The callable a server's call is served through: the function by name,
/// or the method on the object.
fn callable_for(
    req: &mut Request,
    object: Option<Value>,
    name: &[u8],
) -> R<Option<php_ext::abi::Callable>> {
    let n = PhpString::new(req, name)?.into_value();
    let value = match object {
        Some(o) => {
            let mut pair = PhpArray::new(req)?;
            let o = Owner::take(req, o);
            pair.append(req, o)?;
            pair.append(req, n)?;
            pair.into_value()
        }
        None => n,
    };
    let callable = callable_checked(req, &value).ok();
    value.give_up(req);
    Ok(callable)
}

/// Whether the `soapService` has the function `name`: in its function table,
/// or by `__call` for a class or an object.
fn has_function(
    req: &mut Request,
    this: PhpObject,
    kind: ServiceKind,
    object: Option<Value>,
    name: &[u8],
) -> bool {
    let lc = name.to_ascii_lowercase();
    match kind {
        ServiceKind::Class | ServiceKind::Object => {
            let Some(class) = object.and_then(|o| o.as_object()).map(|o| o.class()) else {
                return false;
            };
            php_ext::call::known_method(
                req,
                class,
                &php_ext::decl::Name(lc.as_slice().into()),
                None,
            )
            .is_some()
                || php_ext::call::known_method(
                    req,
                    class,
                    &php_ext::decl::Name(b"__call".as_slice().into()),
                    None,
                )
                .is_some()
        }
        ServiceKind::Functions => {
            let Some(s) = service_ref(req, this) else {
                return false;
            };
            if s.functions_all {
                lookup_function(req, &lc).is_some()
            } else {
                s.functions
                    .as_ref()
                    .is_some_and(|ft| ft.get_str(&lc).is_some())
            }
        }
    }
}

/// The request served: `deserialize_function_call`, the headers'
/// functions, the function, and the response or fault written. The parsed
/// request is freed once it is read, as php's `xmlFreeDoc(doc_request)`
/// follows `deserialize_function_call`.
#[expect(clippy::too_many_lines, reason = "one C function, kept whole")]
fn serve(
    req: &mut Request,
    this: PhpObject,
    sdl: Option<&Sdl>,
    doc_request: NewDoc,
    actor: Option<&[u8]>,
    soap_action: Option<&[u8]>,
    kind: ServiceKind,
    uri: Option<&[u8]>,
) -> Res<()> {
    let ce = Throwables::of(req)?;
    let parsed = deserialize_function_call(req, sdl, doc_request.node(), actor, soap_action);
    drop(doc_request);
    let mut parsed = match parsed {
        Ok(c) => c,
        Err(Error::Thrown(t)) => {
            if matches!(t.cause(req), php_ext::raising::Cause::Throwable(_)) {
                discard(req);
                return server_exception(req, this, sdl, None, t).map_err(Error::from);
            }
            return Err(Error::Thrown(t));
        }
        Err(e) => return Err(e),
    };
    if let Some(s) = service_mut(req, this) {
        s.added_headers = Some(Vec::new());
    }
    let function = parsed.function;
    // The object a `SOAP_CLASS` call made, the call's one owner of it:
    // given up once the function has run (`zval_ptr_dtor(&tmp_soap)`),
    // or on the way out.
    let mut made: Option<Owner<Value>> = None;
    let result = (|| -> Res<()> {
        let soap_obj: Option<Value> = match kind {
            ServiceKind::Object => {
                service_ref(req, this).and_then(|s| s.object.as_deref().copied())
            }
            ServiceKind::Class => {
                let (class, argv, persistence) = match service_ref(req, this) {
                    Some(s) => {
                        let argv: Vec<Value> = s.argv.iter().map(|a| **a).collect();
                        (s.class, argv, s.persistence)
                    }
                    None => (None, Vec::new(), 0),
                };
                let Some(class) = class else {
                    return Ok(());
                };
                let session = persistence == PERSISTENCE_SESSION;
                let mut found = None;
                if session {
                    found = session_object(req, class)?;
                }
                match found {
                    Some(o) => Some(o),
                    None => match php_ext::call::instantiate(req, class, &argv, &[]) {
                        Ok(obj) => {
                            // The object's one owner, moved into the
                            // session or kept as `made` until the call is
                            // done.
                            let obj = obj.into_value();
                            let view = *obj;
                            made = if session {
                                store_session_object(req, obj)?
                            } else {
                                Some(obj)
                            };
                            Some(view)
                        }
                        Err(t) => {
                            discard(req);
                            return server_exception(req, this, sdl, function, t)
                                .map_err(Error::from);
                        }
                    },
                }
            }
            ServiceKind::Functions => None,
        };
        let release_made = |req: &mut Request, made: &mut Option<Owner<Value>>| {
            if let Some(v) = made.take() {
                v.give_up(req);
            }
        };
        // The headers' functions.
        let mut i = 0;
        while i < parsed.headers.len() {
            let fname = parsed.headers[i].function_name.clone().unwrap_or_default();
            if has_function(req, this, kind, soap_obj, &fname) {
                let target = if matches!(kind, ServiceKind::Class | ServiceKind::Object) {
                    soap_obj
                } else {
                    None
                };
                let callable = callable_for(req, target, &fname)?;
                let Some(callable) = callable else {
                    let shown = String::from_utf8_lossy(&fname).into_owned();
                    docref(
                        req,
                        Level::Warning,
                        &format!("Function '{shown}' call failed"),
                    );
                    release_made(req, &mut made);
                    return Ok(());
                };
                let params: Vec<Value> = parsed.headers[i].parameters.iter().map(|p| **p).collect();
                let called = call(req, &callable, &params);
                match called_pending(req, called) {
                    Ok(v) => {
                        let old = std::mem::replace(&mut parsed.headers[i].retval, v);
                        old.give_up(req);
                    }
                    Err(t) => {
                        discard(req);
                        release_made(req, &mut made);
                        return server_exception(req, this, sdl, function, t).map_err(Error::from);
                    }
                }
                let retval = parsed.headers[i].retval.deref();
                if let ValueRef::Object(o) = retval.as_ref()
                    && o.instance_of(req, ce.soap_fault)
                {
                    discard(req);
                    let one = std::slice::from_ref(&parsed.headers[i]);
                    soap_server_fault_ex(req, sdl, function, retval, one)?;
                    release_made(req, &mut made);
                    return Ok(());
                }
            } else if parsed.headers[i].must_understand {
                release_made(req, &mut made);
                return Err(server_fault_en(
                    req,
                    "MustUnderstand",
                    "Header not understood",
                ));
            }
            i += 1;
        }
        // The function.
        let name = parsed.function_name.clone();
        let retval;
        if has_function(req, this, kind, soap_obj, &name) {
            let target = if matches!(kind, ServiceKind::Class | ServiceKind::Object) {
                soap_obj
            } else {
                None
            };
            let Some(callable) = callable_for(req, target, &name)? else {
                let shown = String::from_utf8_lossy(&name).into_owned();
                docref(
                    req,
                    Level::Warning,
                    &format!("Function '{shown}' call failed"),
                );
                release_made(req, &mut made);
                return Ok(());
            };
            let params: Vec<Value> = parsed.params.iter().map(|p| **p).collect();
            // `if (EG(exception))` after the call as well: the memory
            // limit's fatal, raised at the call's entry, leaves php's
            // unwind exit and no answer (8.5.12).
            let called = call(req, &callable, &params);
            let called = called_pending(req, called);
            if kind == ServiceKind::Class {
                release_made(req, &mut made);
            }
            match called {
                Ok(v) => retval = v,
                Err(t) => {
                    if matches!(t.cause(req), php_ext::raising::Cause::Throwable(_)) {
                        discard(req);
                        return server_exception(req, this, sdl, function, t).map_err(Error::from);
                    }
                    return Err(Error::Thrown(t));
                }
            }
        } else {
            release_made(req, &mut made);
            let shown = String::from_utf8_lossy(&name).into_owned();
            return Err(Error::fatal(format!("Function '{shown}' doesn't exist")));
        }
        // php's `addSoapHeader()` additions, after the request's own.
        let added = service_mut(req, this)
            .and_then(|s| s.added_headers.take())
            .unwrap_or_default();
        for v in added {
            parsed.headers.push(HeaderCall {
                function: None,
                function_name: None,
                must_understand: false,
                parameters: Vec::new(),
                retval: v,
                hdr: None,
            });
        }
        if let ValueRef::Object(o) = retval.deref().as_ref()
            && o.instance_of(req, ce.soap_fault)
        {
            discard(req);
            let r = soap_server_fault_ex(req, sdl, function, *retval, &[]);
            retval.give_up(req);
            return r.map_err(Error::from);
        }
        let response_name = if let Some(n) = function.and_then(|f| f.response_name.clone()) {
            n
        } else {
            let mut n = name.clone();
            n.extend_from_slice(b"Response");
            n
        };
        let doc_return = serialize_response_call(
            req,
            sdl,
            function,
            Some(&response_name),
            uri,
            *retval,
            &parsed.headers,
            parsed.version,
        );
        retval.give_up(req);
        let doc_return = match doc_return {
            Ok(d) => d,
            Err(Error::Thrown(t))
                if matches!(t.cause(req), php_ext::raising::Cause::Throwable(_)) =>
            {
                discard(req);
                return server_exception(req, this, sdl, function, t).map_err(Error::from);
            }
            Err(e) => return Err(e),
        };
        discard(req);
        if let Some(doc) = doc_return {
            let (buf, _) = Save::doc(doc.node(), 0, false, None);
            drop(doc);
            let buf = buf.unwrap_or_default();
            if buf.is_empty() {
                return Err(docref_error(req, "Dump memory failed"));
            }
            if parsed.version == SOAP_1_2 {
                req.response_mut().header_add(
                    b"Content-Type: application/soap+xml; charset=utf-8",
                    true,
                    0,
                );
            } else {
                req.response_mut()
                    .header_add(b"Content-Type: text/xml; charset=utf-8", true, 0);
            }
            if output_compression(req) {
                req.response_mut().header_add(b"Connection: close", true, 0);
            } else {
                req.response_mut().header_add(
                    format!("Content-Length: {}", buf.len()).as_bytes(),
                    true,
                    0,
                );
            }
            req.response_mut().write(&buf);
            if let Some(s) = service_mut(req, this)
                && s.trace
            {
                s.last_response_body = Some(buf);
            }
        } else {
            req.response_mut()
                .header_add(b"HTTP/1.1 202 Accepted", true, 0);
            req.response_mut().header_add(b"Content-Length: 0", true, 0);
        }
        Ok(())
    })();
    if let Some(v) = made.take() {
        v.give_up(req);
    }
    if let Some(s) = service_mut(req, this) {
        let left = s.added_headers.take();
        for v in left.into_iter().flatten() {
            v.give_up(req);
        }
    }
    parsed.release(req);
    result
}

/// The key php keeps a session-persistent object under.
const SESSION_KEY: &[u8] = b"_bogus_session_name";

/// A function of the program's served by name, its answer.
fn call_named_function(req: &mut Request, name: &[u8]) -> Res<Owner<Value>> {
    let n = PhpString::new(req, name)?.into_value();
    let callable = php_ext::coerce::to_callable(req, &n);
    n.give_up(req);
    Ok(call(req, &callable?, &[])?)
}

/// The session's `$_SESSION` array, through its reference.
fn session_array(req: &mut Request) -> Option<Value> {
    req.variables_mut().global(b"_SESSION")
}

/// The session-persistent object, the session started first as
/// `php_session_start` does where it is neither active nor disabled; a
/// fault where it was unserialized before its class was loaded.
fn session_object(req: &mut Request, class: ClassId) -> Res<Option<Value>> {
    let answered = call_named_function(req, b"session_status")?;
    let status = answered.as_int().unwrap_or(0);
    answered.give_up(req);
    if status != 2 && status != 0 {
        let started = call_named_function(req, b"session_start")?;
        started.give_up(req);
    }
    let Some(sess) = session_array(req) else {
        return Ok(None);
    };
    let Some(arr) = sess.deref().as_array() else {
        return Ok(None);
    };
    let Some(v) = arr.find(SESSION_KEY).map(|v| v.deref()) else {
        return Ok(None);
    };
    let Some(o) = v.as_object() else {
        return Ok(None);
    };
    if o.class() == class {
        return Ok(Some(v));
    }
    if lookup_class(req, b"__PHP_Incomplete_Class")?.is_some_and(|c| c == o.class()) {
        return Err(server_fault_en(
            req,
            "Server",
            "SoapServer class was deserialized from the session prior to loading the class passed to SoapServer::setClass(). Start the session after loading all classes to resolve this issue.",
        ));
    }
    Ok(None)
}

/// `php_set_session_var`: the object's owner moved into `$_SESSION`, or
/// handed back where there is no session to take it.
fn store_session_object(req: &mut Request, value: Owner<Value>) -> Res<Option<Owner<Value>>> {
    let Some(sess) = session_array(req) else {
        return Ok(Some(value));
    };
    let Some(cell) = sess.as_php_ref() else {
        return Ok(Some(value));
    };
    let mut arr = match cell.separate_array(req) {
        Ok(Some(arr)) => arr,
        Ok(None) => return Ok(Some(value)),
        Err(thrown) => {
            value.give_up(req);
            return Err(thrown.into());
        }
    };
    arr.set_str(req, SESSION_KEY, value)?;
    Ok(None)
}

/// `php_output_discard`: the top buffer dropped, where there is one.
fn discard(req: &mut Request) {
    let level = match req.response_mut().buffer_op(&OutputOp::GetLevel) {
        Ok(v) => {
            let level = v.as_int().unwrap_or(0);
            v.give_up(req);
            level
        }
        Err(_) => 0,
    };
    if level > 0
        && let Ok(v) = req.response_mut().buffer_op(&OutputOp::EndClean)
    {
        v.give_up(req);
    }
}

/// `deserialize_parameters`: the arguments by the description's names,
/// or element by element in order.
fn deserialize_parameters(
    req: &mut Request,
    cx: &mut Cx,
    sdl: Option<&Sdl>,
    params: Option<Node>,
    function: Option<&Function>,
    tables: &mut Tables,
) -> Res<Vec<Owner<Value>>> {
    let mut out: Vec<Owner<Value>> = Vec::new();
    if let Some(f) = function {
        let Some(rp) = &f.request_parameters else {
            return Ok(out);
        };
        let use_names = rp
            .values()
            .any(|p| get_node(params, p.param_name.as_deref().unwrap_or_default(), None).is_some());
        if use_names {
            tables.ask(req, rp.values().count(), false)?;
            for p in rp.values() {
                let val = get_node(params, p.param_name.as_deref().unwrap_or_default(), None);
                match val {
                    None => out.push(Owner::null()),
                    Some(v) => match master_to_zval(req, cx, p.encode, v) {
                        Ok(z) => out.push(z),
                        Err(e) => {
                            release_all(req, out);
                            return Err(e);
                        }
                    },
                }
            }
            return Ok(out);
        }
    }
    let Some(first) = params else {
        return Ok(out);
    };
    let elements: Vec<Node> = std::iter::successors(Some(first), |n| n.next())
        .filter(|n| n.kind() == Kind::Element)
        .collect();
    let num = elements.len();
    let document_with_no_params = num == 1
        && function.is_some_and(|f| {
            soap_binding(sdl, Some(f)).is_some_and(|b| b.style == Style::Document)
                && f.request_parameters.as_ref().is_none_or(Table::is_empty)
                && first.name() == f.function_name
        });
    if document_with_no_params {
        return Ok(out);
    }
    // The table asked for fallibly since 8.5.12, no arguments where the
    // memory limit refused it.
    if num > 0 && !tables.ask(req, num, true)? {
        return Ok(out);
    }
    for (cur, node) in elements.into_iter().enumerate() {
        let mut enc = None;
        if let Some(f) = function {
            let found = f
                .request_parameters
                .as_ref()
                .and_then(|rp| rp.get_int(cur as u64));
            if let Some(p) = found {
                enc = p.encode;
            } else {
                release_all(req, out);
                return Err(server_fault_en(
                    req,
                    "Client",
                    "Error cannot find parameter",
                ));
            }
        }
        match master_to_zval(req, cx, enc, node) {
            Ok(z) => out.push(z),
            Err(e) => {
                release_all(req, out);
                return Err(e);
            }
        }
        // Since 8.5.12 the arguments stop once the memory limit was met,
        // and none is handed on.
        if req.memory().has_oom() {
            release_all(req, out);
            return Ok(Vec::new());
        }
    }
    Ok(out)
}

fn release_all(req: &mut Request, values: Vec<Owner<Value>>) {
    for v in values {
        v.give_up(req);
    }
}

/// The header blocks' arguments and answers given back.
fn release_headers(req: &mut Request, headers: Vec<HeaderCall<'_>>) {
    for h in headers {
        release_all(req, h.parameters);
        h.retval.give_up(req);
    }
}

/// `find_function`: the function the element names, a document-style
/// one only where it takes nothing, else `get_doc_function`; and the
/// name to call.
fn find_function(sdl: Option<&Sdl>, func: Node) -> (Option<&Function>, Vec<u8>) {
    let name = func.name();
    let mut function = get_function(sdl, &name);
    if let Some(f) = function
        && let Some(fnb) = soap_binding(sdl, Some(f))
        && fnb.style == Style::Document
        && (func.children().is_some()
            || f.request_parameters
                .as_ref()
                .is_some_and(|rp| !rp.is_empty()))
    {
        function = None;
    }
    if sdl.is_some() && function.is_none() {
        function = get_doc_function(sdl, Some(func));
    }
    match function {
        Some(f) => (Some(f), f.function_name.clone()),
        None => (None, name),
    }
}

/// `find_function_using_soap_action`.
fn find_function_using_soap_action<'a>(
    sdl: Option<&'a Sdl>,
    soap_action: &[u8],
) -> Option<&'a Function> {
    let sdl = sdl?;
    let mut action = soap_action;
    if action.first() == Some(&b'"') {
        if action.len() < 2 || action.last() != Some(&b'"') {
            return None;
        }
        action = &action[1..action.len() - 1];
    }
    if action.is_empty() {
        return None;
    }
    sdl.functions.values().find(|f| {
        soap_binding(Some(sdl), Some(f))
            .and_then(|fnb| fnb.soap_action.as_deref())
            .is_some_and(|a| a == action)
    })
}

/// `get_envelope`: the first element, which must be a SOAP 1.1 or 1.2
/// envelope.
fn get_envelope(req: &mut Request, doc: Node) -> Res<Option<(Node, i64, &'static [u8])>> {
    let mut trav = doc.children();
    while let Some(t) = trav {
        if t.kind() == Kind::Element {
            if node_is_equal(t, Some(b"Envelope"), Some(SOAP_1_1_ENV_NAMESPACE)) {
                req.state::<State>().soap_version = SOAP_1_1;
                return Ok(Some((t, SOAP_1_1, SOAP_1_1_ENV_NAMESPACE)));
            }
            if node_is_equal(t, Some(b"Envelope"), Some(SOAP_1_2_ENV_NAMESPACE)) {
                req.state::<State>().soap_version = SOAP_1_2;
                return Ok(Some((t, SOAP_1_2, SOAP_1_2_ENV_NAMESPACE)));
            }
            return Err(server_fault_en(req, "VersionMismatch", "Wrong Version"));
        }
        trav = t.next();
    }
    Ok(None)
}

/// The `encodingStyle` checks php makes on the envelope, the body and the
/// header: unqualified attributes refused (on the body only in 1.2), and
/// an `encodingStyle` other than SOAP 1.1's.
fn check_env_attributes(
    req: &mut Request,
    node: Node,
    version: i64,
    what: &str,
    unqualified_always: bool,
) -> Res<()> {
    let mut attr = node.properties();
    while let Some(a) = attr {
        if a.ns().is_none() {
            if unqualified_always || version == SOAP_1_2 {
                return Err(server_fault_en(
                    req,
                    "Client",
                    &format!(
                        "A SOAP {what} element cannot have non Namespace qualified attributes"
                    ),
                ));
            }
        } else if crate::xml::attr_is_equal(a, b"encodingStyle", Some(SOAP_1_2_ENV_NAMESPACE)) {
            if version == SOAP_1_2 {
                return Err(server_fault_en(
                    req,
                    "Client",
                    &format!("encodingStyle cannot be specified on the {what}"),
                ));
            } else if attr_content(a) != SOAP_1_1_ENC_NAMESPACE {
                return Err(server_fault_en(
                    req,
                    "Client",
                    "Unknown data encoding style",
                ));
            }
        }
        attr = a.next();
    }
    Ok(())
}

/// The next element from `trav` on.
fn next_element(mut trav: Option<Node>) -> Option<Node> {
    while let Some(t) = trav {
        if t.kind() == Kind::Element {
            return Some(t);
        }
        trav = t.next();
    }
    None
}

/// `deserialize_function_call`: the envelope read, the function found,
/// its headers and its arguments decoded.
fn deserialize_function_call<'a>(
    req: &mut Request,
    sdl: Option<&'a Sdl>,
    request: Node,
    actor: Option<&[u8]>,
    soap_action: Option<&[u8]>,
) -> Res<Call<'a>> {
    let mut cx = Cx::default();
    encode_reset_ns(req, &mut cx);
    let r = deserialize_function_call_in(req, &mut cx, sdl, request, actor, soap_action);
    encode_finish(req, &mut cx);
    r
}

#[expect(clippy::too_many_lines, reason = "one C function, kept whole")]
fn deserialize_function_call_in<'a>(
    req: &mut Request,
    cx: &mut Cx,
    sdl: Option<&'a Sdl>,
    request: Node,
    actor: Option<&[u8]>,
    soap_action: Option<&[u8]>,
) -> Res<Call<'a>> {
    let Some((env, version, envelope_ns)) = get_envelope(req, request)? else {
        return Err(server_fault_en(
            req,
            "Client",
            "looks like we got XML without \"Envelope\" element",
        ));
    };
    check_env_attributes(req, env, version, "Envelope", true)?;
    let mut head = None;
    let mut trav = next_element(env.children());
    if let Some(t) = trav
        && node_is_equal(t, Some(b"Header"), Some(envelope_ns))
    {
        head = Some(t);
        trav = t.next();
    }
    let mut body = None;
    trav = next_element(trav);
    if let Some(t) = trav
        && node_is_equal(t, Some(b"Body"), Some(envelope_ns))
    {
        body = Some(t);
        trav = t.next();
    }
    trav = next_element(trav);
    let Some(body) = body else {
        return Err(server_fault_en(
            req,
            "Client",
            "Body must be present in a SOAP envelope",
        ));
    };
    check_env_attributes(req, body, version, "Body", false)?;
    if trav.is_some() && version == SOAP_1_2 {
        return Err(server_fault_en(
            req,
            "Client",
            "A SOAP 1.2 envelope can contain only Header and Body",
        ));
    }
    let func = next_element(body.children());
    let mut function: Option<&Function> = None;
    let mut function_name: Vec<u8> = Vec::new();
    if let Some(a) = soap_action {
        function = find_function_using_soap_action(sdl, a);
        if let Some(f) = function {
            function_name.clone_from(&f.function_name);
        }
    }
    match func {
        None => {
            if function.is_none() {
                function = get_doc_function(sdl, None);
                match function {
                    Some(f) => function_name.clone_from(&f.function_name),
                    None => {
                        return Err(server_fault_en(
                            req,
                            "Client",
                            "looks like we got \"Body\" without function call",
                        ));
                    }
                }
            }
        }
        Some(fnode) => {
            if version == SOAP_1_1 {
                if let Some(a) = get_attribute(
                    fnode.properties(),
                    b"encodingStyle",
                    Some(SOAP_1_1_ENV_NAMESPACE),
                ) && attr_content(a) != SOAP_1_1_ENC_NAMESPACE
                {
                    return Err(server_fault_en(
                        req,
                        "Client",
                        "Unknown Data Encoding Style",
                    ));
                }
            } else if let Some(a) = get_attribute(
                fnode.properties(),
                b"encodingStyle",
                Some(SOAP_1_2_ENV_NAMESPACE),
            ) && attr_content(a) != SOAP_1_2_ENC_NAMESPACE
            {
                return Err(server_fault_en(
                    req,
                    "DataEncodingUnknown",
                    "Unknown Data Encoding Style",
                ));
            }
            if function.is_none() {
                let (f, n) = find_function(sdl, fnode);
                function = f;
                function_name = n;
            }
            if sdl.is_some() && function.is_none() {
                if version == SOAP_1_2 {
                    return Err(server_fault_en(
                        req,
                        "rpc:ProcedureNotPresent",
                        "Procedure not present",
                    ));
                }
                let shown = String::from_utf8_lossy(&fnode.name()).into_owned();
                return Err(Error::fatal(format!("Procedure '{shown}' not present")));
            }
        }
    }
    let mut headers: Vec<HeaderCall<'a>> = Vec::new();
    let mut tables = Tables::default();
    // Since 8.5.12 the headers stop once the memory limit was met, and the
    // call is found no function, the headers read so far kept.
    let mut oom = false;
    // The blocks read so far are given back where a later one fails.
    let read = (|| -> Res<()> {
        let Some(head) = head else {
            return Ok(());
        };
        check_env_attributes(req, head, version, "Header", true)?;
        let mut trav = head.children();
        while let Some(hdr_func) = trav {
            trav = hdr_func.next();
            if hdr_func.kind() != Kind::Element {
                continue;
            }
            if version == SOAP_1_1 {
                if let Some(a) = get_attribute(
                    hdr_func.properties(),
                    b"encodingStyle",
                    Some(SOAP_1_1_ENV_NAMESPACE),
                ) && attr_content(a) != SOAP_1_1_ENC_NAMESPACE
                {
                    return Err(server_fault_en(
                        req,
                        "Client",
                        "Unknown Data Encoding Style",
                    ));
                }
                if let Some(a) = get_attribute(hdr_func.properties(), b"actor", Some(envelope_ns)) {
                    let c = attr_content(a);
                    if c != SOAP_1_1_ACTOR_NEXT && actor.is_none_or(|ac| c != ac) {
                        continue;
                    }
                }
            } else {
                if let Some(a) = get_attribute(
                    hdr_func.properties(),
                    b"encodingStyle",
                    Some(SOAP_1_2_ENV_NAMESPACE),
                ) && attr_content(a) != SOAP_1_2_ENC_NAMESPACE
                {
                    return Err(server_fault_en(
                        req,
                        "DataEncodingUnknown",
                        "Unknown Data Encoding Style",
                    ));
                }
                if let Some(a) = get_attribute(hdr_func.properties(), b"role", Some(envelope_ns)) {
                    let c = attr_content(a);
                    if c != SOAP_1_2_ACTOR_UNLIMATERECEIVER
                        && c != SOAP_1_2_ACTOR_NEXT
                        && actor.is_none_or(|ac| c != ac)
                    {
                        continue;
                    }
                }
            }
            let mut must_understand = false;
            if let Some(a) =
                get_attribute(hdr_func.properties(), b"mustUnderstand", Some(envelope_ns))
            {
                match attr_content(a).as_slice() {
                    b"1" | b"true" => must_understand = true,
                    b"0" | b"false" => must_understand = false,
                    _ => {
                        return Err(server_fault_en(
                            req,
                            "Client",
                            "mustUnderstand value is not boolean",
                        ));
                    }
                }
            }
            let (hfunction, hname) = find_function(sdl, hdr_func);
            let mut hdr = None;
            if hfunction.is_none()
                && sdl.is_some()
                && let Some(fnb) = soap_binding(sdl, function)
                && let Some(ih) = &fnb.input.headers
            {
                let mut key = Vec::new();
                if let Some(ns) = hdr_func.ns() {
                    key.extend_from_slice(&crate::xml::href(ns));
                    key.push(b':');
                }
                key.extend_from_slice(&hname);
                hdr = ih.get_str(&key);
            }
            let parameters = if let Some(h) = hdr {
                // `h->parameters = emalloc(sizeof(zval))`.
                tables.ask(req, 1, false)?;
                vec![master_to_zval(req, cx, h.encode, hdr_func)?]
            } else {
                let mut node = Some(hdr_func);
                if let Some(fnb) = soap_binding(sdl, hfunction)
                    && fnb.style == Style::Rpc
                {
                    node = hdr_func.children();
                }
                deserialize_parameters(req, cx, sdl, node, hfunction, &mut tables)?
            };
            headers.push(HeaderCall {
                function: hfunction,
                function_name: Some(hname),
                must_understand,
                parameters,
                retval: Owner::null(),
                hdr,
            });
            if req.memory().has_oom() {
                oom = true;
                return Ok(());
            }
        }
        Ok(())
    })();
    if let Err(e) = read {
        release_headers(req, headers);
        tables.free(req);
        return Err(e);
    }
    if oom {
        // `find_function` named the call before the headers were read.
        if function.is_none()
            && function_name.is_empty()
            && let Some(f) = func
        {
            function_name = f.name();
        }
        return Ok(Call {
            function: None,
            function_name,
            params: Vec::new(),
            version,
            headers,
            tables,
        });
    }
    let mut params_node = func;
    if let Some(fnb) = soap_binding(sdl, function) {
        if fnb.style == Style::Rpc {
            params_node = func.and_then(Node::children);
        }
    } else {
        params_node = func.and_then(Node::children);
    }
    let params = match deserialize_parameters(req, cx, sdl, params_node, function, &mut tables) {
        Ok(params) => params,
        Err(e) => {
            release_headers(req, headers);
            tables.free(req);
            return Err(e);
        }
    };
    if function.is_none()
        && function_name.is_empty()
        && let Some(f) = func
    {
        function_name = f.name();
    }
    Ok(Call {
        function,
        function_name,
        params,
        version,
        headers,
        tables,
    })
}
