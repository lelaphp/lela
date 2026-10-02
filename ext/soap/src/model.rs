//! The description of a WSDL, `php_sdl.h` and `php_encoding.h` as Rust
//! types: one [`Sdl`] holds its types, encoders, bindings and functions,
//! each type and encoder in an arena of its own and named by index, so the
//! graph `php_schema.c` builds (an element shared by reference, a type
//! that contains itself, an encoder pointing back at its type) is plain
//! data. Nothing in it is a PHP value, so a parsed description is
//! `Send + Sync` and is shared as `Arc<Sdl>`; the per-call state php keeps
//! in `SOAP_GLOBAL` is [`crate::State`]'s.

use crate::table::Table;

// ---- namespaces and type names, `php_encoding.h`

pub(crate) const XSD_1999_NAMESPACE: &[u8] = b"http://www.w3.org/1999/XMLSchema";
pub(crate) const SOAP_1_1_ENV_NAMESPACE: &[u8] = b"http://schemas.xmlsoap.org/soap/envelope/";
pub(crate) const SOAP_1_1_ENV_NS_PREFIX: &[u8] = b"SOAP-ENV";
pub(crate) const SOAP_1_2_ENV_NAMESPACE: &[u8] = b"http://www.w3.org/2003/05/soap-envelope";
pub(crate) const SOAP_1_2_ENV_NS_PREFIX: &[u8] = b"env";
pub(crate) const SOAP_1_1_ENC_NAMESPACE: &[u8] = b"http://schemas.xmlsoap.org/soap/encoding/";
pub(crate) const SOAP_1_1_ENC_NS_PREFIX: &[u8] = b"SOAP-ENC";
pub(crate) const SOAP_1_2_ENC_NAMESPACE: &[u8] = b"http://www.w3.org/2003/05/soap-encoding";
pub(crate) const SOAP_1_2_ENC_NS_PREFIX: &[u8] = b"enc";
pub(crate) const XSD_DRAFT_2000_NAMESPACE: &[u8] = b"http://www.w3.org/2000/10/XMLSchema";
pub(crate) const XSD_NAMESPACE: &[u8] = b"http://www.w3.org/2001/XMLSchema";
pub(crate) const XSD_NS_PREFIX: &[u8] = b"xsd";
pub(crate) const XSI_NAMESPACE: &[u8] = b"http://www.w3.org/2001/XMLSchema-instance";
pub(crate) const XSI_NS_PREFIX: &[u8] = b"xsi";
pub(crate) const XML_NAMESPACE: &[u8] = b"http://www.w3.org/XML/1998/namespace";
pub(crate) const XML_NS_PREFIX: &[u8] = b"xml";
pub(crate) const APACHE_NAMESPACE: &[u8] = b"http://xml.apache.org/xml-soap";
pub(crate) const WSDL_NAMESPACE: &[u8] = b"http://schemas.xmlsoap.org/wsdl/";
pub(crate) const WSDL_NS_PREFIX: &[u8] = b"wsdl";
pub(crate) const WSDL_SOAP11_NAMESPACE: &[u8] = b"http://schemas.xmlsoap.org/wsdl/soap/";
pub(crate) const WSDL_SOAP12_NAMESPACE: &[u8] = b"http://schemas.xmlsoap.org/wsdl/soap12/";
pub(crate) const RPC_SOAP12_NAMESPACE: &[u8] = b"http://www.w3.org/2003/05/soap-rpc";
pub(crate) const RPC_SOAP12_NS_PREFIX: &[u8] = b"rpc";
pub(crate) const WSDL_HTTP11_NAMESPACE: &[u8] = b"http://schemas.xmlsoap.org/wsdl/http/";
pub(crate) const WSDL_HTTP12_NAMESPACE: &[u8] = b"http://www.w3.org/2003/05/soap/bindings/HTTP/";
pub(crate) const WSDL_HTTP_NS_PREFIX: &[u8] = b"http";
pub(crate) const WSDL_HTTP_TRANSPORT: &[u8] = b"http://schemas.xmlsoap.org/soap/http";
pub(crate) const WSDL_MIME_NAMESPACE: &[u8] = b"http://schemas.xmlsoap.org/wsdl/mime/";
pub(crate) const WSDL_DIME_NAMESPACE: &[u8] = b"http://schemas.xmlsoap.org/ws/2002/04/dime/wsdl/";
pub(crate) const WSDL_DIME_OPEN: &[u8] = b"http://schemas.xmlsoap.org/ws/2002/04/dime/open-layout";
pub(crate) const WSDL_DIME_CLOSED: &[u8] =
    b"http://schemas.xmlsoap.org/ws/2002/04/dime/closed-layout";
pub(crate) const SOAP_1_1_ACTOR_NEXT: &[u8] = b"http://schemas.xmlsoap.org/soap/actor/next";
pub(crate) const SOAP_1_2_ACTOR_NEXT: &[u8] = b"http://www.w3.org/2003/05/soap-envelope/role/next";
pub(crate) const SOAP_1_2_ACTOR_NONE: &[u8] = b"http://www.w3.org/2003/05/soap-envelope/role/none";
pub(crate) const SOAP_1_2_ACTOR_UNLIMATERECEIVER: &[u8] =
    b"http://www.w3.org/2003/05/soap-envelope/role/ultimateReceiver";

/// The type numbers of `php_encoding.h` and the zval types php's
/// default table indexes beside them.
pub(crate) mod ty {
    pub(crate) const IS_NULL: i32 = 1;
    pub(crate) const IS_FALSE: i32 = 2;
    pub(crate) const IS_TRUE: i32 = 3;
    pub(crate) const IS_LONG: i32 = 4;
    pub(crate) const IS_DOUBLE: i32 = 5;
    pub(crate) const IS_STRING: i32 = 6;
    pub(crate) const IS_ARRAY: i32 = 7;
    pub(crate) const IS_OBJECT: i32 = 8;
    pub(crate) const XSD_STRING: i32 = 101;
    pub(crate) const XSD_BOOLEAN: i32 = 102;
    pub(crate) const XSD_DECIMAL: i32 = 103;
    pub(crate) const XSD_FLOAT: i32 = 104;
    pub(crate) const XSD_DOUBLE: i32 = 105;
    pub(crate) const XSD_DURATION: i32 = 106;
    pub(crate) const XSD_DATETIME: i32 = 107;
    pub(crate) const XSD_TIME: i32 = 108;
    pub(crate) const XSD_DATE: i32 = 109;
    pub(crate) const XSD_GYEARMONTH: i32 = 110;
    pub(crate) const XSD_GYEAR: i32 = 111;
    pub(crate) const XSD_GMONTHDAY: i32 = 112;
    pub(crate) const XSD_GDAY: i32 = 113;
    pub(crate) const XSD_GMONTH: i32 = 114;
    pub(crate) const XSD_HEXBINARY: i32 = 115;
    pub(crate) const XSD_BASE64BINARY: i32 = 116;
    pub(crate) const XSD_ANYURI: i32 = 117;
    pub(crate) const XSD_QNAME: i32 = 118;
    pub(crate) const XSD_NOTATION: i32 = 119;
    pub(crate) const XSD_NORMALIZEDSTRING: i32 = 120;
    pub(crate) const XSD_TOKEN: i32 = 121;
    pub(crate) const XSD_LANGUAGE: i32 = 122;
    pub(crate) const XSD_NMTOKEN: i32 = 123;
    pub(crate) const XSD_NAME: i32 = 124;
    pub(crate) const XSD_NCNAME: i32 = 125;
    pub(crate) const XSD_ID: i32 = 126;
    pub(crate) const XSD_IDREF: i32 = 127;
    pub(crate) const XSD_IDREFS: i32 = 128;
    pub(crate) const XSD_ENTITY: i32 = 129;
    pub(crate) const XSD_ENTITIES: i32 = 130;
    pub(crate) const XSD_INTEGER: i32 = 131;
    pub(crate) const XSD_NONPOSITIVEINTEGER: i32 = 132;
    pub(crate) const XSD_NEGATIVEINTEGER: i32 = 133;
    pub(crate) const XSD_LONG: i32 = 134;
    pub(crate) const XSD_INT: i32 = 135;
    pub(crate) const XSD_SHORT: i32 = 136;
    pub(crate) const XSD_BYTE: i32 = 137;
    pub(crate) const XSD_NONNEGATIVEINTEGER: i32 = 138;
    pub(crate) const XSD_UNSIGNEDLONG: i32 = 139;
    pub(crate) const XSD_UNSIGNEDINT: i32 = 140;
    pub(crate) const XSD_UNSIGNEDSHORT: i32 = 141;
    pub(crate) const XSD_UNSIGNEDBYTE: i32 = 142;
    pub(crate) const XSD_POSITIVEINTEGER: i32 = 143;
    pub(crate) const XSD_NMTOKENS: i32 = 144;
    pub(crate) const XSD_ANYTYPE: i32 = 145;
    pub(crate) const XSD_UR_TYPE: i32 = 146;
    pub(crate) const XSD_ANYXML: i32 = 147;
    pub(crate) const APACHE_MAP: i32 = 200;
    pub(crate) const SOAP_ENC_ARRAY: i32 = 300;
    pub(crate) const SOAP_ENC_OBJECT: i32 = 301;
    pub(crate) const XSD_1999_TIMEINSTANT: i32 = 401;
    pub(crate) const UNKNOWN_TYPE: i32 = 999_998;
    pub(crate) const END_KNOWN_TYPES: i32 = 999_999;
}

// ---- encoders, `php_encoding.h`

/// The `to_zval` of an `encode`: which of `php_encoding.c`'s decoders it
/// is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToZval {
    /// `guess_zval_convert`.
    Guess,
    Null,
    String,
    /// `to_zval_stringr`: whitespace replaced.
    StringR,
    /// `to_zval_stringc`: whitespace collapsed.
    StringC,
    Long,
    Double,
    Bool,
    Array,
    Object,
    Map,
    Base64,
    Hexbin,
    Any,
    /// `sdl_guess_convert_zval`: by the encoder's SDL type.
    SdlGuess,
    /// `to_zval_user`: a typemap's `from_xml`.
    User,
}

/// The `to_xml` of an `encode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToXml {
    /// `guess_xml_convert`.
    Guess,
    Null,
    String,
    Long,
    Double,
    Bool,
    /// `guess_array_map`.
    GuessArrayMap,
    Array,
    Object,
    Map,
    /// `to_xml_list1`.
    List1,
    Datetime,
    Time,
    Date,
    GYearMonth,
    GYear,
    GMonthDay,
    GDay,
    GMonth,
    Duration,
    Base64,
    Hexbin,
    Any,
    /// `sdl_guess_convert_xml`.
    SdlGuess,
    /// `to_xml_user`: a typemap's `to_xml`.
    User,
}

/// `encodeType`, without `map`, which only a typemap's encoder carries
/// ([`crate::State`]'s typemap).
#[derive(Debug, Clone, Default)]
pub(crate) struct EncodeType {
    pub(crate) type_: i32,
    pub(crate) type_str: Option<Vec<u8>>,
    pub(crate) ns: Option<Vec<u8>>,
    /// `{ns}type_str`, where both are set.
    pub(crate) clark_notation: Option<Vec<u8>>,
    pub(crate) sdl_type: Option<TypeId>,
}

/// `encode`.
#[derive(Debug, Clone)]
pub(crate) struct Encoder {
    pub(crate) details: EncodeType,
    pub(crate) to_zval: ToZval,
    pub(crate) to_xml: ToXml,
}

/// One row of `defaultEncoding[]`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Builtin {
    pub(crate) type_: i32,
    pub(crate) type_str: Option<&'static [u8]>,
    pub(crate) ns: Option<&'static [u8]>,
    pub(crate) to_zval: ToZval,
    pub(crate) to_xml: ToXml,
}

const fn b(
    type_: i32,
    type_str: &'static [u8],
    ns: &'static [u8],
    to_zval: ToZval,
    to_xml: ToXml,
) -> Builtin {
    Builtin {
        type_,
        type_str: Some(type_str),
        ns: Some(ns),
        to_zval,
        to_xml,
    }
}

const X: &[u8] = XSD_NAMESPACE;
const X99: &[u8] = XSD_1999_NAMESPACE;

/// `defaultEncoding[]`, in php's order, which decides which row a type
/// number and a `ns:type` name find first.
pub(crate) static DEFAULT_ENCODING: &[Builtin] = &[
    Builtin {
        type_: ty::UNKNOWN_TYPE,
        type_str: None,
        ns: None,
        to_zval: ToZval::Guess,
        to_xml: ToXml::Guess,
    },
    b(
        ty::IS_NULL,
        b"nil",
        XSI_NAMESPACE,
        ToZval::Null,
        ToXml::Null,
    ),
    b(ty::IS_STRING, b"string", X, ToZval::String, ToXml::String),
    b(ty::IS_LONG, b"int", X, ToZval::Long, ToXml::Long),
    b(ty::IS_DOUBLE, b"float", X, ToZval::Double, ToXml::Double),
    b(ty::IS_FALSE, b"boolean", X, ToZval::Bool, ToXml::Bool),
    b(ty::IS_TRUE, b"boolean", X, ToZval::Bool, ToXml::Bool),
    b(
        ty::IS_ARRAY,
        b"Array",
        SOAP_1_1_ENC_NAMESPACE,
        ToZval::Array,
        ToXml::GuessArrayMap,
    ),
    b(
        ty::IS_OBJECT,
        b"Struct",
        SOAP_1_1_ENC_NAMESPACE,
        ToZval::Object,
        ToXml::Object,
    ),
    b(
        ty::IS_ARRAY,
        b"Array",
        SOAP_1_2_ENC_NAMESPACE,
        ToZval::Array,
        ToXml::GuessArrayMap,
    ),
    b(
        ty::IS_OBJECT,
        b"Struct",
        SOAP_1_2_ENC_NAMESPACE,
        ToZval::Object,
        ToXml::Object,
    ),
    b(ty::XSD_STRING, b"string", X, ToZval::String, ToXml::String),
    b(ty::XSD_BOOLEAN, b"boolean", X, ToZval::Bool, ToXml::Bool),
    b(
        ty::XSD_DECIMAL,
        b"decimal",
        X,
        ToZval::StringC,
        ToXml::String,
    ),
    b(ty::XSD_FLOAT, b"float", X, ToZval::Double, ToXml::Double),
    b(ty::XSD_DOUBLE, b"double", X, ToZval::Double, ToXml::Double),
    b(
        ty::XSD_DATETIME,
        b"dateTime",
        X,
        ToZval::StringC,
        ToXml::Datetime,
    ),
    b(ty::XSD_TIME, b"time", X, ToZval::StringC, ToXml::Time),
    b(ty::XSD_DATE, b"date", X, ToZval::StringC, ToXml::Date),
    b(
        ty::XSD_GYEARMONTH,
        b"gYearMonth",
        X,
        ToZval::StringC,
        ToXml::GYearMonth,
    ),
    b(ty::XSD_GYEAR, b"gYear", X, ToZval::StringC, ToXml::GYear),
    b(
        ty::XSD_GMONTHDAY,
        b"gMonthDay",
        X,
        ToZval::StringC,
        ToXml::GMonthDay,
    ),
    b(ty::XSD_GDAY, b"gDay", X, ToZval::StringC, ToXml::GDay),
    b(ty::XSD_GMONTH, b"gMonth", X, ToZval::StringC, ToXml::GMonth),
    b(
        ty::XSD_DURATION,
        b"duration",
        X,
        ToZval::StringC,
        ToXml::Duration,
    ),
    b(
        ty::XSD_HEXBINARY,
        b"hexBinary",
        X,
        ToZval::Hexbin,
        ToXml::Hexbin,
    ),
    b(
        ty::XSD_BASE64BINARY,
        b"base64Binary",
        X,
        ToZval::Base64,
        ToXml::Base64,
    ),
    b(ty::XSD_LONG, b"long", X, ToZval::Long, ToXml::Long),
    b(ty::XSD_INT, b"int", X, ToZval::Long, ToXml::Long),
    b(ty::XSD_SHORT, b"short", X, ToZval::Long, ToXml::Long),
    b(ty::XSD_BYTE, b"byte", X, ToZval::Long, ToXml::Long),
    b(
        ty::XSD_NONPOSITIVEINTEGER,
        b"nonPositiveInteger",
        X,
        ToZval::Long,
        ToXml::Long,
    ),
    b(
        ty::XSD_POSITIVEINTEGER,
        b"positiveInteger",
        X,
        ToZval::Long,
        ToXml::Long,
    ),
    b(
        ty::XSD_NONNEGATIVEINTEGER,
        b"nonNegativeInteger",
        X,
        ToZval::Long,
        ToXml::Long,
    ),
    b(
        ty::XSD_NEGATIVEINTEGER,
        b"negativeInteger",
        X,
        ToZval::Long,
        ToXml::Long,
    ),
    b(
        ty::XSD_UNSIGNEDBYTE,
        b"unsignedByte",
        X,
        ToZval::Long,
        ToXml::Long,
    ),
    b(
        ty::XSD_UNSIGNEDSHORT,
        b"unsignedShort",
        X,
        ToZval::Long,
        ToXml::Long,
    ),
    b(
        ty::XSD_UNSIGNEDINT,
        b"unsignedInt",
        X,
        ToZval::Long,
        ToXml::Long,
    ),
    b(
        ty::XSD_UNSIGNEDLONG,
        b"unsignedLong",
        X,
        ToZval::Long,
        ToXml::Long,
    ),
    b(ty::XSD_INTEGER, b"integer", X, ToZval::Long, ToXml::Long),
    b(ty::XSD_ANYTYPE, b"anyType", X, ToZval::Guess, ToXml::Guess),
    b(ty::XSD_UR_TYPE, b"ur-type", X, ToZval::Guess, ToXml::Guess),
    b(ty::XSD_ANYURI, b"anyURI", X, ToZval::StringC, ToXml::String),
    b(ty::XSD_QNAME, b"QName", X, ToZval::StringC, ToXml::String),
    b(
        ty::XSD_NOTATION,
        b"NOTATION",
        X,
        ToZval::StringC,
        ToXml::String,
    ),
    b(
        ty::XSD_NORMALIZEDSTRING,
        b"normalizedString",
        X,
        ToZval::StringR,
        ToXml::String,
    ),
    b(ty::XSD_TOKEN, b"token", X, ToZval::StringC, ToXml::String),
    b(
        ty::XSD_LANGUAGE,
        b"language",
        X,
        ToZval::StringC,
        ToXml::String,
    ),
    b(
        ty::XSD_NMTOKEN,
        b"NMTOKEN",
        X,
        ToZval::StringC,
        ToXml::String,
    ),
    b(
        ty::XSD_NMTOKENS,
        b"NMTOKENS",
        X,
        ToZval::StringC,
        ToXml::List1,
    ),
    b(ty::XSD_NAME, b"Name", X, ToZval::StringC, ToXml::String),
    b(ty::XSD_NCNAME, b"NCName", X, ToZval::StringC, ToXml::String),
    b(ty::XSD_ID, b"ID", X, ToZval::StringC, ToXml::String),
    b(ty::XSD_IDREF, b"IDREF", X, ToZval::StringC, ToXml::String),
    b(ty::XSD_IDREFS, b"IDREFS", X, ToZval::StringC, ToXml::List1),
    b(ty::XSD_ENTITY, b"ENTITY", X, ToZval::StringC, ToXml::String),
    b(
        ty::XSD_ENTITIES,
        b"ENTITIES",
        X,
        ToZval::StringC,
        ToXml::List1,
    ),
    b(
        ty::APACHE_MAP,
        b"Map",
        APACHE_NAMESPACE,
        ToZval::Map,
        ToXml::Map,
    ),
    b(
        ty::SOAP_ENC_OBJECT,
        b"Struct",
        SOAP_1_1_ENC_NAMESPACE,
        ToZval::Object,
        ToXml::Object,
    ),
    b(
        ty::SOAP_ENC_ARRAY,
        b"Array",
        SOAP_1_1_ENC_NAMESPACE,
        ToZval::Array,
        ToXml::Array,
    ),
    b(
        ty::SOAP_ENC_OBJECT,
        b"Struct",
        SOAP_1_2_ENC_NAMESPACE,
        ToZval::Object,
        ToXml::Object,
    ),
    b(
        ty::SOAP_ENC_ARRAY,
        b"Array",
        SOAP_1_2_ENC_NAMESPACE,
        ToZval::Array,
        ToXml::Array,
    ),
    b(
        ty::XSD_STRING,
        b"string",
        X99,
        ToZval::String,
        ToXml::String,
    ),
    b(ty::XSD_BOOLEAN, b"boolean", X99, ToZval::Bool, ToXml::Bool),
    b(
        ty::XSD_DECIMAL,
        b"decimal",
        X99,
        ToZval::StringC,
        ToXml::String,
    ),
    b(ty::XSD_FLOAT, b"float", X99, ToZval::Double, ToXml::Double),
    b(
        ty::XSD_DOUBLE,
        b"double",
        X99,
        ToZval::Double,
        ToXml::Double,
    ),
    b(ty::XSD_LONG, b"long", X99, ToZval::Long, ToXml::Long),
    b(ty::XSD_INT, b"int", X99, ToZval::Long, ToXml::Long),
    b(ty::XSD_SHORT, b"short", X99, ToZval::Long, ToXml::Long),
    b(ty::XSD_BYTE, b"byte", X99, ToZval::Long, ToXml::Long),
    b(
        ty::XSD_1999_TIMEINSTANT,
        b"timeInstant",
        X99,
        ToZval::StringC,
        ToXml::String,
    ),
    b(
        ty::XSD_ANYXML,
        b"<anyXML>",
        b"<anyXML>",
        ToZval::Any,
        ToXml::Any,
    ),
];

/// Which SOAP encoding namespace a default encoder was found under by
/// `get_encoder`'s fallback from `SOAP-ENC:type` to `xsd:type`, the copy
/// php adds to `sdl->encoders` with the namespace it was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EncNs {
    Soap11,
    Soap12,
}

impl EncNs {
    pub(crate) fn href(self) -> &'static [u8] {
        match self {
            EncNs::Soap11 => SOAP_1_1_ENC_NAMESPACE,
            EncNs::Soap12 => SOAP_1_2_ENC_NAMESPACE,
        }
    }
}

/// An `encodePtr`: a row of [`DEFAULT_ENCODING`] (with the namespace
/// `get_encoder` copied it under), an encoder of the description's
/// arena, or one of the typemap the current call runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EncRef {
    Default(u16),
    DefaultIn(u16, EncNs),
    Sdl(EncId),
    Typemap(u32),
}

/// An index into [`Sdl::encoder_arena`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct EncId(pub(crate) u32);

/// An index into [`Sdl::type_arena`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TypeId(pub(crate) u32);

// ---- the description, `php_sdl.h`

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindingType {
    Soap = 1,
    Http = 2,
}

/// `sdlEncodingStyle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    Rpc = 1,
    Document = 2,
}

/// `sdlRpcEncodingStyle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RpcEncoding {
    Default = 0,
    Soap11 = 1,
    Soap12 = 2,
}

/// `sdlEncodingUse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Use {
    /// The zero `memset` leaves: an operation's `<input>` or `<output>`
    /// whose binding had no `<soap:body>`, neither `encoded` nor `literal`.
    Unset = 0,
    Encoded = 1,
    Literal = 2,
}

/// `sdl`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Sdl {
    /// By lowercased function name.
    pub(crate) functions: Table<Function>,
    pub(crate) types: Option<Table<TypeId>>,
    pub(crate) elements: Option<Table<TypeId>>,
    pub(crate) encoders: Option<Table<EncId>>,
    pub(crate) bindings: Option<Table<Binding>>,
    /// The functions by request element name: indexes into `functions`,
    /// in its order.
    pub(crate) requests: Option<Table<usize>>,
    pub(crate) groups: Option<Table<TypeId>>,
    pub(crate) target_ns: Option<Vec<u8>>,
    pub(crate) source: Option<Vec<u8>>,
    /// Every `sdlType` the description made.
    pub(crate) type_arena: Vec<Type>,
    /// Every `encode` the description made.
    pub(crate) encoder_arena: Vec<Encoder>,
}

impl Sdl {
    pub(crate) fn ty(&self, id: TypeId) -> Option<&Type> {
        self.type_arena.get(id.0 as usize)
    }

    pub(crate) fn ty_mut(&mut self, id: TypeId) -> Option<&mut Type> {
        self.type_arena.get_mut(id.0 as usize)
    }

    pub(crate) fn new_type(&mut self, t: Type) -> TypeId {
        let id = TypeId(u32::try_from(self.type_arena.len()).unwrap_or(u32::MAX));
        self.type_arena.push(t);
        id
    }

    pub(crate) fn encoder(&self, id: EncId) -> Option<&Encoder> {
        self.encoder_arena.get(id.0 as usize)
    }

    pub(crate) fn encoder_mut(&mut self, id: EncId) -> Option<&mut Encoder> {
        self.encoder_arena.get_mut(id.0 as usize)
    }

    pub(crate) fn new_encoder(&mut self, e: Encoder) -> EncId {
        let id = EncId(u32::try_from(self.encoder_arena.len()).unwrap_or(u32::MAX));
        self.encoder_arena.push(e);
        id
    }

    /// The binding a function runs over, `function->binding`.
    pub(crate) fn binding_of(&self, function: &Function) -> Option<&Binding> {
        self.bindings.as_ref()?.nth(function.binding?)
    }

    /// The function a request element names, `sdl->requests`.
    pub(crate) fn request(&self, name: &[u8]) -> Option<&Function> {
        let at = *self.requests.as_ref()?.get_str(name)?;
        self.functions.nth(at)
    }
}

/// `sdlBinding`.
#[derive(Debug, Clone)]
pub(crate) struct Binding {
    pub(crate) name: Vec<u8>,
    pub(crate) location: Option<Vec<u8>>,
    pub(crate) binding_type: BindingType,
    pub(crate) attributes: Option<SoapBinding>,
}

/// `sdlSoapBinding`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SoapBinding {
    pub(crate) style: Style,
    pub(crate) transport: i32,
}

/// `sdlSoapBindingFunctionHeader`.
#[derive(Debug, Clone)]
pub(crate) struct Header {
    pub(crate) name: Vec<u8>,
    pub(crate) ns: Option<Vec<u8>>,
    pub(crate) use_: Use,
    pub(crate) encoding_style: RpcEncoding,
    pub(crate) element: Option<TypeId>,
    pub(crate) encode: Option<EncRef>,
    pub(crate) headerfaults: Option<Table<Header>>,
}

/// `sdlSoapBindingFunctionFault`.
#[derive(Debug, Clone)]
pub(crate) struct SoapFaultBinding {
    pub(crate) ns: Option<Vec<u8>>,
    pub(crate) use_: Use,
    pub(crate) encoding_style: RpcEncoding,
}

/// `sdlSoapBindingFunctionBody`.
#[derive(Debug, Clone)]
pub(crate) struct Body {
    pub(crate) ns: Option<Vec<u8>>,
    pub(crate) use_: Use,
    pub(crate) encoding_style: RpcEncoding,
    pub(crate) headers: Option<Table<Header>>,
}

impl Default for Body {
    fn default() -> Body {
        Body {
            ns: None,
            use_: Use::Unset,
            encoding_style: RpcEncoding::Default,
            headers: None,
        }
    }
}

/// `sdlSoapBindingFunction`.
#[derive(Debug, Clone)]
pub(crate) struct SoapBindingFunction {
    pub(crate) soap_action: Option<Vec<u8>>,
    pub(crate) style: Style,
    pub(crate) input: Body,
    pub(crate) output: Body,
}

/// `sdlRestrictionInt`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RestrictionInt {
    pub(crate) value: i32,
    pub(crate) fixed: bool,
}

/// `sdlRestrictionChar`.
#[derive(Debug, Clone, Default)]
pub(crate) struct RestrictionChar {
    pub(crate) value: Vec<u8>,
    pub(crate) fixed: bool,
}

/// `sdlRestrictions`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Restrictions {
    pub(crate) enumeration: Option<Table<RestrictionChar>>,
    pub(crate) min_exclusive: Option<RestrictionInt>,
    pub(crate) min_inclusive: Option<RestrictionInt>,
    pub(crate) max_exclusive: Option<RestrictionInt>,
    pub(crate) max_inclusive: Option<RestrictionInt>,
    pub(crate) total_digits: Option<RestrictionInt>,
    pub(crate) fraction_digits: Option<RestrictionInt>,
    pub(crate) length: Option<RestrictionInt>,
    pub(crate) min_length: Option<RestrictionInt>,
    pub(crate) max_length: Option<RestrictionInt>,
    pub(crate) white_space: Option<RestrictionChar>,
    pub(crate) pattern: Option<RestrictionChar>,
}

/// `sdlContentKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentKind {
    Element = 0,
    Sequence = 1,
    All = 2,
    Choice = 3,
    GroupRef = 4,
    Group = 5,
    Any = 6,
}

/// `sdlContentModel`'s union.
#[derive(Debug, Clone)]
pub(crate) enum Content {
    /// `XSD_CONTENT_ELEMENT`: the element.
    Element(TypeId),
    /// `XSD_CONTENT_GROUP`: the group.
    Group(TypeId),
    /// `XSD_CONTENT_SEQUENCE`, `_ALL` and `_CHOICE`: the particles.
    Particles(Vec<ContentModel>),
    /// `XSD_CONTENT_GROUP_REF`: the group's name, until pass two resolves
    /// it into a `Group`.
    GroupRef(Vec<u8>),
    /// `XSD_CONTENT_ANY`.
    None,
}

/// `sdlContentModel`.
#[derive(Debug, Clone)]
pub(crate) struct ContentModel {
    pub(crate) kind: ContentKind,
    pub(crate) min_occurs: i32,
    pub(crate) max_occurs: i32,
    pub(crate) u: Content,
}

/// `sdlTypeKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum TypeKind {
    #[default]
    Simple = 0,
    List = 1,
    Union = 2,
    Complex = 3,
    Restriction = 4,
    Extension = 5,
}

/// `sdlUse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum AttrUse {
    #[default]
    Default = 0,
    Optional = 1,
    Prohibited = 2,
    Required = 3,
}

/// `sdlForm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Form {
    #[default]
    Default = 0,
    Qualified = 1,
    Unqualified = 2,
}

/// `sdlType`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Type {
    pub(crate) kind: TypeKind,
    pub(crate) nillable: bool,
    pub(crate) name: Option<Vec<u8>>,
    pub(crate) namens: Option<Vec<u8>>,
    pub(crate) elements: Option<Table<TypeId>>,
    pub(crate) attributes: Option<Table<Attribute>>,
    pub(crate) restrictions: Option<Box<Restrictions>>,
    pub(crate) encode: Option<EncRef>,
    pub(crate) model: Option<Box<ContentModel>>,
    pub(crate) def: Option<Vec<u8>>,
    pub(crate) fixed: Option<Vec<u8>>,
    pub(crate) ref_: Option<Vec<u8>>,
    pub(crate) form: Form,
}

/// `sdlParam`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Param {
    pub(crate) order: i32,
    pub(crate) element: Option<TypeId>,
    pub(crate) encode: Option<EncRef>,
    pub(crate) param_name: Option<Vec<u8>>,
}

/// `sdlFault`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Fault {
    pub(crate) name: Vec<u8>,
    pub(crate) details: Option<Table<Param>>,
    pub(crate) binding: Option<SoapFaultBinding>,
}

/// `sdlFunction`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Function {
    pub(crate) function_name: Vec<u8>,
    pub(crate) request_name: Option<Vec<u8>>,
    pub(crate) response_name: Option<Vec<u8>>,
    pub(crate) request_parameters: Option<Table<Param>>,
    pub(crate) response_parameters: Option<Table<Param>>,
    /// The binding, by its row in [`Sdl::bindings`], in order: two
    /// ports over one `<binding>` make two rows of one name, the second
    /// pushed (`zend_hash_next_index_insert`), which a name would not
    /// tell apart.
    pub(crate) binding: Option<usize>,
    pub(crate) binding_attributes: Option<SoapBindingFunction>,
    pub(crate) faults: Option<Table<Fault>>,
}

/// `sdlExtraAttribute`.
#[derive(Debug, Clone, Default)]
pub(crate) struct ExtraAttribute {
    pub(crate) ns: Option<Vec<u8>>,
    pub(crate) val: Option<Vec<u8>>,
}

/// `sdlAttribute`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Attribute {
    pub(crate) name: Option<Vec<u8>>,
    pub(crate) namens: Option<Vec<u8>>,
    pub(crate) ref_: Option<Vec<u8>>,
    pub(crate) def: Option<Vec<u8>>,
    pub(crate) fixed: Option<Vec<u8>>,
    pub(crate) form: Form,
    pub(crate) use_: AttrUse,
    pub(crate) extra_attributes: Option<Table<ExtraAttribute>>,
    pub(crate) encode: Option<EncRef>,
}

// ---- the default encoders' indexes, `php_soap_prepare_globals`

impl Builtin {
    /// The row as an `encode` of its own.
    pub(crate) fn encoder(&self) -> Encoder {
        let clark = match (self.ns, self.type_str) {
            (Some(ns), Some(t)) => {
                let mut c = Vec::with_capacity(ns.len() + t.len() + 2);
                c.push(b'{');
                c.extend_from_slice(ns);
                c.push(b'}');
                c.extend_from_slice(t);
                Some(c)
            }
            _ => None,
        };
        Encoder {
            details: EncodeType {
                type_: self.type_,
                type_str: self.type_str.map(<[u8]>::to_vec),
                ns: self.ns.map(<[u8]>::to_vec),
                clark_notation: clark,
                sdl_type: None,
            },
            to_zval: self.to_zval,
            to_xml: self.to_xml,
        }
    }
}

/// The row of [`DEFAULT_ENCODING`] at `index`.
pub(crate) fn builtin(index: u16) -> Option<&'static Builtin> {
    DEFAULT_ENCODING.get(usize::from(index))
}

/// `php_soap_defEncIndex`: the first row of a type number.
pub(crate) fn default_by_type(type_: i32) -> Option<u16> {
    DEFAULT_ENCODING
        .iter()
        .position(|b| b.type_ == type_)
        .and_then(|at| u16::try_from(at).ok())
}

/// `php_soap_defEnc`: the first row named `ns:type`, or `type` for a row
/// with no namespace.
pub(crate) fn default_by_name(nscat: &[u8]) -> Option<u16> {
    DEFAULT_ENCODING
        .iter()
        .position(|b| match (b.type_str, b.ns) {
            (Some(t), Some(ns)) => {
                nscat.len() == ns.len() + 1 + t.len()
                    && nscat.starts_with(ns)
                    && nscat[ns.len()] == b':'
                    && nscat.ends_with(t)
            }
            (Some(t), None) => nscat == t,
            _ => false,
        })
        .and_then(|at| u16::try_from(at).ok())
}

/// `get_conversion`: the default encoder of a type number, none for a
/// number php's table does not have (php's `Encoding: Cannot find
/// encoding`, the caller's to raise).
pub(crate) fn get_conversion(type_: i32) -> Option<EncRef> {
    default_by_type(type_).map(EncRef::Default)
}

/// `php_soap_defEncNs`: the prefix php writes for a namespace it knows.
pub(crate) fn default_ns_prefix(ns: &[u8]) -> Option<&'static [u8]> {
    [
        (XSD_1999_NAMESPACE, XSD_NS_PREFIX),
        (XSD_NAMESPACE, XSD_NS_PREFIX),
        (XSI_NAMESPACE, XSI_NS_PREFIX),
        (XML_NAMESPACE, XML_NS_PREFIX),
        (SOAP_1_1_ENC_NAMESPACE, SOAP_1_1_ENC_NS_PREFIX),
        (SOAP_1_2_ENC_NAMESPACE, SOAP_1_2_ENC_NS_PREFIX),
    ]
    .iter()
    .find(|(n, _)| *n == ns)
    .map(|(_, p)| *p)
}
