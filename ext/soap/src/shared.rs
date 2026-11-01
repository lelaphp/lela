//! What the layers share about the objects `soap.c` reads by slot:
//! `SoapClient`'s 36 private properties (`Z_CLIENT_*_P`), `SoapServer`'s
//! one, `SoapParam`'s, `SoapHeader`'s and `SoapVar`'s (`Z_PARAM_*_P`,
//! `Z_HEADER_*_P`, `Z_VAR_ENC_*_P`), and the faults every layer adds.

use std::sync::Arc;

use php_ext::call::{lookup_class, new_object};
use php_ext::raising::{Thrown, fatal};
use php_ext::request::Request;
use php_ext::values::{ClassId, Owner, PhpObject, Value, ValueRef};

use crate::classes::{Fault, FaultSlot, SdlPayload, set_soap_fault};
use crate::model::Sdl;
use crate::throwables::Throwables;

type R<T> = Result<T, Thrown>;

/// `SoapClient`'s slots, `php_soap.h`'s `Z_CLIENT_*_P`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientSlot {
    Uri = 0,
    Style = 1,
    Use = 2,
    Location = 3,
    Trace = 4,
    Compression = 5,
    Sdl = 6,
    Typemap = 7,
    HttpSocket = 8,
    HttpUrl = 9,
    Login = 10,
    Password = 11,
    UseDigest = 12,
    Digest = 13,
    ProxyHost = 14,
    ProxyPort = 15,
    ProxyLogin = 16,
    ProxyPassword = 17,
    Exceptions = 18,
    Encoding = 19,
    Classmap = 20,
    Features = 21,
    ConnectionTimeout = 22,
    StreamCtx = 23,
    UserAgent = 24,
    KeepAlive = 25,
    SslMethod = 26,
    SoapVersion = 27,
    UseProxy = 28,
    Cookies = 29,
    DefaultHeaders = 30,
    SoapFault = 31,
    LastRequest = 32,
    LastResponse = 33,
    LastRequestHeaders = 34,
    LastResponseHeaders = 35,
}

/// `SoapParam`'s slots.
pub(crate) const PARAM_NAME: u32 = 0;
pub(crate) const PARAM_DATA: u32 = 1;

/// `SoapHeader`'s slots.
pub(crate) const HEADER_NAMESPACE: u32 = 0;
pub(crate) const HEADER_NAME: u32 = 1;
pub(crate) const HEADER_DATA: u32 = 2;
pub(crate) const HEADER_MUST_UNDERSTAND: u32 = 3;
pub(crate) const HEADER_ACTOR: u32 = 4;

/// `SoapVar`'s slots.
pub(crate) const VAR_ENC_TYPE: u32 = 0;
pub(crate) const VAR_ENC_VALUE: u32 = 1;
pub(crate) const VAR_ENC_STYPE: u32 = 2;
pub(crate) const VAR_ENC_NS: u32 = 3;
pub(crate) const VAR_ENC_NAME: u32 = 4;
pub(crate) const VAR_ENC_NAMENS: u32 = 5;

/// `SoapServer`'s one slot, `Z_SERVER_SOAP_FAULT_P`.
pub(crate) const SERVER_SOAP_FAULT: u32 = 0;

/// A declared slot as it is, dereferenced (`php_soap_deref`), with no
/// owner taken: `null` for one never written.
pub(crate) fn slot(object: PhpObject, index: u32) -> Value {
    object.slot(index).map(|v| v.deref()).unwrap_or_default()
}

/// A client slot, as [`slot`].
pub(crate) fn client_slot(object: PhpObject, which: ClientSlot) -> Value {
    slot(object, which as u32)
}

/// `Z_TYPE_P(Z_CLIENT_EXCEPTIONS_P(client)) != IS_FALSE`: whether the
/// client throws its faults rather than returning them.
pub(crate) fn client_throws(client: PhpObject) -> bool {
    !matches!(
        client_slot(client, ClientSlot::Exceptions).as_ref(),
        ValueRef::Bool(false)
    )
}

/// A client slot written: `value` is the slot's from here.
pub(crate) fn set_client_slot(
    req: &mut Request,
    object: PhpObject,
    which: ClientSlot,
    value: Owner<Value>,
) -> R<()> {
    object.set_slot(req, which as u32, value)
}

/// Whether a value is `null` or unset.
pub(crate) fn is_null(value: &Value) -> bool {
    matches!(value.deref().as_ref(), ValueRef::Null | ValueRef::Undef)
}

/// The class named, linked by the time any native runs.
pub(crate) fn class_id(req: &mut Request, name: &str) -> R<ClassId> {
    match lookup_class(req, name.as_bytes())? {
        Some(class) => Ok(class),
        None => Err(fatal(req, &format!("InternalError: {name} is not linked"))),
    }
}

/// Whether `object` is an instance of the class named.
pub(crate) fn instance_of(req: &mut Request, object: PhpObject, name: &str) -> bool {
    match lookup_class(req, name.as_bytes()) {
        Ok(Some(class)) => object.instance_of(req, class),
        _ => false,
    }
}

/// `object_init_ex(soap_fault_class_entry)` and `set_soap_fault`: a
/// fresh fault, the caller's one owner.
pub(crate) fn new_fault(req: &mut Request, f: &Fault<'_>) -> R<Owner<PhpObject>> {
    let class = Throwables::of(req)?.soap_fault;
    let object = new_object(req, class)?;
    set_soap_fault(req, *object, f)?;
    Ok(object)
}

/// `add_soap_fault_ex`: a fresh fault in `lang`, kept in the client's
/// `__soap_fault` or the server's; the fault answered with one more owner,
/// the caller's.
pub(crate) fn add_soap_fault_ex(
    req: &mut Request,
    obj: PhpObject,
    code: &[u8],
    string: &[u8],
    actor: Option<&[u8]>,
    detail: Option<Value>,
    lang: &[u8],
) -> R<Owner<PhpObject>> {
    let fault = new_fault(
        req,
        &Fault {
            code_ns: None,
            code: Some(code),
            string: Some(string),
            actor,
            detail,
            name: None,
            lang,
        },
    )?;
    let index = if instance_of(req, obj, "SoapClient") {
        ClientSlot::SoapFault as u32
    } else {
        SERVER_SOAP_FAULT
    };
    let kept = Owner::take(req, Value::object(*fault));
    if let Err(thrown) = obj.set_slot(req, index, kept) {
        fault.give_up(req);
        return Err(thrown);
    }
    Ok(fault)
}

/// `add_soap_fault`: [`add_soap_fault_ex`] for a caller that only keeps
/// the fault where it is kept, the caller's owner given up,
/// `zval_ptr_dtor(&fault)`. The fault's message shares its
/// `$faultstring`, as [`set_soap_fault`] writes it; a fault made first
/// leaves `soap_lang_en` standing, which php interns at `PHP_MINIT`.
/// The call's bytes are counted past an `Exception`'s, whose `$file`
/// and trace take what the file's path takes:
///
/// ```php
/// $o = ['location' => 'ftp://127.0.0.1/x', 'uri' => 'urn:x', 'exceptions' => false];
/// $a = new SoapClient(null, $o); $r = $a->__soapCall('f', []); $r = null;
/// $b = new SoapClient(null, $o);
/// $m = memory_get_usage(); $e = new Exception('x'); $x = memory_get_usage() - $m;
/// $m = memory_get_usage(); $r = $b->__soapCall('f', []); echo memory_get_usage() - $m - $x, "\n";
/// echo $r->faultcode, ': ', $r->faultstring, "\n";
/// ```
/// ```expect
/// 2144
/// HTTP: Unknown protocol. Only http and https are allowed.
/// ```
pub(crate) fn add_soap_fault(
    req: &mut Request,
    obj: PhpObject,
    code: &[u8],
    string: &[u8],
    actor: Option<&[u8]>,
    detail: Option<Value>,
    lang: &[u8],
) -> R<()> {
    let fault = add_soap_fault_ex(req, obj, code, string, actor, detail, lang)?;
    fault.give_up(req);
    Ok(())
}

/// One of `SoapFault`'s slots, as [`slot`].
pub(crate) fn fault_slot(object: PhpObject, which: FaultSlot) -> Value {
    slot(object, which.index())
}

/// A `Soap\Sdl` holding `sdl`, the caller's one owner.
pub(crate) fn new_sdl_object(req: &mut Request, sdl: Arc<Sdl>) -> R<Owner<PhpObject>> {
    let class = class_id(req, "Soap\\Sdl")?;
    let object = new_object(req, class)?;
    if let Some(p) = object.native_mut::<SdlPayload>(req) {
        p.sdl = Some(sdl);
    }
    Ok(object)
}

/// The description a `Soap\Sdl` value holds.
pub(crate) fn sdl_of(req: &Request, value: &Value) -> Option<Arc<Sdl>> {
    let object = value.deref().as_object()?;
    object.native::<SdlPayload>(req)?.sdl.clone()
}
