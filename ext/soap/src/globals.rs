//! `SOAP_GLOBAL` and what a layer raises.

use std::sync::Arc;

use php_ext::raising::{Thrown, fatal, fatal_while_pending};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, Value};

use crate::SOAP_1_1;
use crate::model::{Encoder, Sdl};
use crate::table::Table;

/// `SOAP_GLOBAL`: request-scoped (ADR-0005), what php keeps per thread
/// and resets in `PHP_RINIT_FUNCTION(soap)`. Nothing in it is shared or
/// synchronised; a client or a server sets what a call runs under and
/// puts back what it found, as `SOAP_CLIENT_BEGIN_CODE` and its kin do.
/// The `ref_map` php keeps beside these is the encoding layer's own, held
/// by the call that serialises.
pub(crate) struct State {
    /// `soap_version`: what a fault's code is mapped by.
    pub(crate) soap_version: i64,
    /// `use_soap_error_handler`.
    pub(crate) use_soap_error_handler: bool,
    /// `error_code`: `Client`, `Server` or `WSDL` while one is running.
    pub(crate) error_code: Option<&'static str>,
    /// `error_object`: the client or the server running.
    pub(crate) error_object: Option<PhpObject>,
    /// `sdl`: the description the running call encodes against.
    pub(crate) sdl: Option<Arc<Sdl>>,
    /// `encoding`: the name of the `encoding` option, which strings are
    /// converted to and from UTF-8 by.
    pub(crate) encoding: Option<Vec<u8>>,
    /// `class_map`: the `classmap` option, borrowed for the call.
    pub(crate) class_map: Option<Value>,
    /// `features`.
    pub(crate) features: i64,
    /// `typemap`: the typemap the running call converts under.
    pub(crate) typemap: Option<Typemap>,
    /// `cur_uniq_ns`: the last `nsN` prefix made.
    pub(crate) cur_uniq_ns: i64,
    /// `cur_uniq_ref`: the last `refN` id made.
    pub(crate) cur_uniq_ref: i64,
    /// `mem_cache`: the descriptions read in this request by their URI,
    /// each with the time it was read, for `WSDL_CACHE_MEMORY`; in
    /// insertion order, which decides the one `soap.wsdl_cache_limit`
    /// evicts among those read in the same second.
    /// What a `__get` answered the encoder, one owner each, held until
    /// the serialisation that read it ends (`encode_finish`).
    pub(crate) held: Vec<Owner<Value>>,
    /// A throwable the encoder left pending and went on past
    /// (`zend_value_error` in a converter), handed on once the message
    /// is made.
    pub(crate) pending: Option<Thrown>,
    pub(crate) mem_cache: Table<(Arc<Sdl>, i64)>,
}

impl Default for State {
    fn default() -> State {
        State {
            soap_version: SOAP_1_1,
            use_soap_error_handler: false,
            error_code: None,
            error_object: None,
            sdl: None,
            encoding: None,
            class_map: None,
            features: 0,
            typemap: None,
            cur_uniq_ns: 0,
            cur_uniq_ref: 0,
            held: Vec::new(),
            pending: None,
            mem_cache: Table::new(),
        }
    }
}

/// One encoder of a typemap, as `soap_create_typemap` makes it: the
/// encoder it copies, and the program's `to_xml` and `from_xml`, held by
/// the client or the server the typemap is an option of.
#[derive(Debug, Clone)]
pub(crate) struct TypemapEntry {
    pub(crate) enc: Encoder,
    pub(crate) to_xml: Option<Value>,
    pub(crate) to_zval: Option<Value>,
}

/// A typemap, by `ns:type`.
pub(crate) type Typemap = Table<TypemapEntry>;

/// What a layer of this crate raises: a program's throwable, or php's
/// `soap_error` at `E_ERROR`, carried to the native's boundary and raised
/// there ([`Error::raise`]) through soap's error handler, which makes a
/// running client's or server's fatal its fault
/// ([`crate::server::soap_error_handler`]).
#[derive(Debug)]
pub(crate) enum Error {
    Thrown(Thrown),
    /// The message, `SOAP-ERROR: ` and what php formats after it, and
    /// the throwable php had pending when it raised it, which a stream
    /// wrapper threw while a WSDL or a schema was read.
    Fatal {
        message: String,
        pending: Option<Thrown>,
    },
}

impl Error {
    /// A fatal with nothing pending.
    pub(crate) fn fatal(message: String) -> Error {
        Error::Fatal {
            message,
            pending: None,
        }
    }

    /// The error raised at the native's boundary: a throwable handed on,
    /// a fatal raised at `E_ERROR` after what was pending, which soap's
    /// handler sees under the `error_code` and `error_object` standing.
    pub(crate) fn raise(self, req: &mut Request) -> Thrown {
        match self {
            Error::Thrown(t) => t,
            Error::Fatal {
                message,
                pending: Some(pending),
            } => fatal_while_pending(req, pending, &message),
            Error::Fatal {
                message,
                pending: None,
            } => fatal(req, &message),
        }
    }
}

impl From<Thrown> for Error {
    fn from(t: Thrown) -> Error {
        Error::Thrown(t)
    }
}

pub(crate) type Res<T> = Result<T, Error>;

/// `soap_error0(E_ERROR, ...)` and its kin: `SOAP-ERROR: ` and the text.
pub(crate) fn soap_error(text: impl std::fmt::Display) -> Error {
    Error::fatal(format!("SOAP-ERROR: {text}"))
}

/// A native's [`Res`] at its boundary, its fatal raised ([`Error::raise`]).
pub(crate) fn raised<T>(req: &mut Request, r: Res<T>) -> Result<T, Thrown> {
    r.map_err(|e| e.raise(req))
}
