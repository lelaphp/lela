//! libxml2's C interface as the platform's headers declare it at 2.9.13
//! (`libxml/tree.h`, `parser.h`, `HTMLparser.h`, `xpath.h`,
//! `xpathInternals.h`, `xmlerror.h`, `xmlsave.h`, `c14n.h`, `valid.h`,
//! `entities.h`, `hash.h`, `uri.h`, `xinclude.h`): the structs php's
//! libxml and dom read laid out as C lays them out, and the functions
//! they call. `php_libxml.h` includes `tree.h` and `parser.h`, so what
//! it hands an extension that includes it is this as much as its own
//! declarations; the layout is held by this crate's tests.
#![allow(non_camel_case_types, non_snake_case, dead_code, missing_docs)]

use std::ffi::{c_char, c_double, c_int, c_long, c_uint, c_ulong, c_ushort, c_void};

pub(crate) type xmlChar = u8;

/// `xmlElementType`.
pub(crate) const XML_ELEMENT_NODE: c_int = 1;
pub(crate) const XML_ATTRIBUTE_NODE: c_int = 2;
pub(crate) const XML_TEXT_NODE: c_int = 3;
pub(crate) const XML_CDATA_SECTION_NODE: c_int = 4;
pub(crate) const XML_ENTITY_REF_NODE: c_int = 5;
pub(crate) const XML_ENTITY_NODE: c_int = 6;
pub(crate) const XML_PI_NODE: c_int = 7;
pub(crate) const XML_COMMENT_NODE: c_int = 8;
pub(crate) const XML_DOCUMENT_NODE: c_int = 9;
pub(crate) const XML_DOCUMENT_TYPE_NODE: c_int = 10;
pub(crate) const XML_DOCUMENT_FRAG_NODE: c_int = 11;
pub(crate) const XML_NOTATION_NODE: c_int = 12;
pub(crate) const XML_HTML_DOCUMENT_NODE: c_int = 13;
pub(crate) const XML_DTD_NODE: c_int = 14;
pub(crate) const XML_ELEMENT_DECL: c_int = 15;
pub(crate) const XML_ATTRIBUTE_DECL: c_int = 16;
pub(crate) const XML_ENTITY_DECL: c_int = 17;
pub(crate) const XML_NAMESPACE_DECL: c_int = 18;
pub(crate) const XML_XINCLUDE_START: c_int = 19;
pub(crate) const XML_XINCLUDE_END: c_int = 20;

/// `xmlNsType`'s one value.
pub(crate) const XML_LOCAL_NAMESPACE: c_int = XML_NAMESPACE_DECL;

/// `xmlAttributeType`.
pub(crate) const XML_ATTRIBUTE_CDATA: c_int = 1;
pub const XML_ATTRIBUTE_ID: c_int = 2;
pub(crate) const XML_ATTRIBUTE_IDREF: c_int = 3;
pub(crate) const XML_ATTRIBUTE_IDREFS: c_int = 4;
pub(crate) const XML_ATTRIBUTE_ENTITY: c_int = 5;
pub(crate) const XML_ATTRIBUTE_ENTITIES: c_int = 6;
pub(crate) const XML_ATTRIBUTE_NMTOKEN: c_int = 7;
pub(crate) const XML_ATTRIBUTE_NMTOKENS: c_int = 8;
pub(crate) const XML_ATTRIBUTE_ENUMERATION: c_int = 9;
pub(crate) const XML_ATTRIBUTE_NOTATION: c_int = 10;

/// `xmlEntityType`.
pub(crate) const XML_INTERNAL_GENERAL_ENTITY: c_int = 1;
pub(crate) const XML_EXTERNAL_GENERAL_PARSED_ENTITY: c_int = 2;
pub const XML_EXTERNAL_GENERAL_UNPARSED_ENTITY: c_int = 3;
pub(crate) const XML_INTERNAL_PARAMETER_ENTITY: c_int = 4;
pub(crate) const XML_EXTERNAL_PARAMETER_ENTITY: c_int = 5;
pub(crate) const XML_INTERNAL_PREDEFINED_ENTITY: c_int = 6;

/// `xmlErrorLevel`.
pub(crate) const XML_ERR_NONE: c_int = 0;
pub(crate) const XML_ERR_WARNING: c_int = 1;
pub(crate) const XML_ERR_ERROR: c_int = 2;
pub(crate) const XML_ERR_FATAL: c_int = 3;

/// `xmlErrorDomain`.
pub(crate) const XML_FROM_NONE: c_int = 0;
pub(crate) const XML_FROM_PARSER: c_int = 1;
pub(crate) const XML_FROM_TREE: c_int = 2;
pub(crate) const XML_FROM_NAMESPACE: c_int = 3;
pub(crate) const XML_FROM_DTD: c_int = 4;
pub(crate) const XML_FROM_HTML: c_int = 5;
pub(crate) const XML_FROM_MEMORY: c_int = 6;
pub(crate) const XML_FROM_OUTPUT: c_int = 7;
pub(crate) const XML_FROM_IO: c_int = 8;
pub(crate) const XML_FROM_FTP: c_int = 9;
pub(crate) const XML_FROM_HTTP: c_int = 10;
pub(crate) const XML_FROM_XINCLUDE: c_int = 11;
pub(crate) const XML_FROM_XPATH: c_int = 12;
pub(crate) const XML_FROM_XPOINTER: c_int = 13;
pub(crate) const XML_FROM_REGEXP: c_int = 14;
pub(crate) const XML_FROM_DATATYPE: c_int = 15;
pub(crate) const XML_FROM_SCHEMASP: c_int = 16;
pub(crate) const XML_FROM_SCHEMASV: c_int = 17;
pub(crate) const XML_FROM_RELAXNGP: c_int = 18;
pub(crate) const XML_FROM_RELAXNGV: c_int = 19;
pub(crate) const XML_FROM_CATALOG: c_int = 20;
pub(crate) const XML_FROM_C14N: c_int = 21;
pub(crate) const XML_FROM_XSLT: c_int = 22;
pub(crate) const XML_FROM_VALID: c_int = 23;
pub(crate) const XML_FROM_CHECK: c_int = 24;
pub(crate) const XML_FROM_WRITER: c_int = 25;
pub(crate) const XML_FROM_MODULE: c_int = 26;
pub(crate) const XML_FROM_I18N: c_int = 27;
pub(crate) const XML_FROM_SCHEMATRONV: c_int = 28;
pub(crate) const XML_FROM_BUFFER: c_int = 29;
pub(crate) const XML_FROM_URI: c_int = 30;

/// `XML_ERR_INTERNAL_ERROR`, the code `php_list_set_error_structure`
/// gives an error php raises itself.
pub(crate) const XML_ERR_INTERNAL_ERROR: c_int = 1;

/// `xmlParserOption`.
pub const XML_PARSE_RECOVER: c_int = 1 << 0;
pub const XML_PARSE_NOENT: c_int = 1 << 1;
pub const XML_PARSE_DTDLOAD: c_int = 1 << 2;
pub const XML_PARSE_DTDATTR: c_int = 1 << 3;
pub const XML_PARSE_DTDVALID: c_int = 1 << 4;
pub const XML_PARSE_NOERROR: c_int = 1 << 5;
pub const XML_PARSE_NOWARNING: c_int = 1 << 6;
pub const XML_PARSE_PEDANTIC: c_int = 1 << 7;
pub const XML_PARSE_NOBLANKS: c_int = 1 << 8;
pub const XML_PARSE_XINCLUDE: c_int = 1 << 10;
pub const XML_PARSE_NONET: c_int = 1 << 11;
pub const XML_PARSE_NSCLEAN: c_int = 1 << 13;
pub const XML_PARSE_NOCDATA: c_int = 1 << 14;
pub const XML_PARSE_COMPACT: c_int = 1 << 16;
pub const XML_PARSE_HUGE: c_int = 1 << 19;
pub const XML_PARSE_BIG_LINES: c_int = 1 << 22;

/// `htmlParserOption`.
pub const HTML_PARSE_RECOVER: c_int = 1 << 0;
pub const HTML_PARSE_NODEFDTD: c_int = 1 << 2;
pub const HTML_PARSE_NOERROR: c_int = 1 << 5;
pub const HTML_PARSE_NOWARNING: c_int = 1 << 6;
pub const HTML_PARSE_NOBLANKS: c_int = 1 << 8;
pub const HTML_PARSE_NONET: c_int = 1 << 11;
pub const HTML_PARSE_NOIMPLIED: c_int = 1 << 13;

/// `xmlSaveOption`.
pub const XML_SAVE_FORMAT: c_int = 1 << 0;
pub const XML_SAVE_NO_DECL: c_int = 1 << 1;
pub const XML_SAVE_NO_EMPTY: c_int = 1 << 2;
pub const XML_SAVE_AS_XML: c_int = 1 << 5;

/// `xmlXPathObjectType`.
pub(crate) const XPATH_UNDEFINED: c_int = 0;
pub(crate) const XPATH_NODESET: c_int = 1;
pub(crate) const XPATH_BOOLEAN: c_int = 2;
pub(crate) const XPATH_NUMBER: c_int = 3;
pub(crate) const XPATH_STRING: c_int = 4;
pub(crate) const XPATH_XSLT_TREE: c_int = 9;

/// `xmlC14NMode`.
pub(crate) const XML_C14N_1_0: c_int = 0;
pub(crate) const XML_C14N_EXCLUSIVE_1_0: c_int = 1;

/// `XML_XML_NAMESPACE`, NUL-terminated.
pub(crate) const XML_XML_NAMESPACE: &[u8] = b"http://www.w3.org/XML/1998/namespace\0";

/// `struct _xmlNs`.
#[repr(C)]
pub(crate) struct xmlNs {
    pub(crate) next: *mut xmlNs,
    pub(crate) type_: c_int,
    pub(crate) href: *const xmlChar,
    pub(crate) prefix: *const xmlChar,
    pub(crate) private: *mut c_void,
    pub(crate) context: *mut xmlDoc,
}

/// `struct _xmlNode`: every node type's common prefix, and the element's
/// tail.
#[repr(C)]
pub(crate) struct xmlNode {
    pub(crate) private: *mut c_void,
    pub(crate) type_: c_int,
    pub(crate) name: *const xmlChar,
    pub(crate) children: *mut xmlNode,
    pub(crate) last: *mut xmlNode,
    pub(crate) parent: *mut xmlNode,
    pub(crate) next: *mut xmlNode,
    pub(crate) prev: *mut xmlNode,
    pub(crate) doc: *mut xmlDoc,
    pub(crate) ns: *mut xmlNs,
    pub(crate) content: *mut xmlChar,
    pub(crate) properties: *mut xmlAttr,
    pub(crate) nsDef: *mut xmlNs,
    pub(crate) psvi: *mut c_void,
    pub(crate) line: c_ushort,
    pub(crate) extra: c_ushort,
}

/// `struct _xmlAttr`.
#[repr(C)]
pub(crate) struct xmlAttr {
    pub(crate) private: *mut c_void,
    pub(crate) type_: c_int,
    pub(crate) name: *const xmlChar,
    pub(crate) children: *mut xmlNode,
    pub(crate) last: *mut xmlNode,
    pub(crate) parent: *mut xmlNode,
    pub(crate) next: *mut xmlAttr,
    pub(crate) prev: *mut xmlAttr,
    pub(crate) doc: *mut xmlDoc,
    pub(crate) ns: *mut xmlNs,
    pub(crate) atype: c_int,
    pub(crate) psvi: *mut c_void,
}

/// `struct _xmlDoc`.
#[repr(C)]
pub(crate) struct xmlDoc {
    pub(crate) private: *mut c_void,
    pub(crate) type_: c_int,
    pub(crate) name: *mut c_char,
    pub(crate) children: *mut xmlNode,
    pub(crate) last: *mut xmlNode,
    pub(crate) parent: *mut xmlNode,
    pub(crate) next: *mut xmlNode,
    pub(crate) prev: *mut xmlNode,
    pub(crate) doc: *mut xmlDoc,
    pub(crate) compression: c_int,
    pub(crate) standalone: c_int,
    pub(crate) intSubset: *mut xmlDtd,
    pub(crate) extSubset: *mut xmlDtd,
    pub(crate) oldNs: *mut xmlNs,
    pub(crate) version: *const xmlChar,
    pub(crate) encoding: *const xmlChar,
    pub(crate) ids: *mut c_void,
    pub(crate) refs: *mut c_void,
    pub(crate) URL: *const xmlChar,
    pub(crate) charset: c_int,
    pub(crate) dict: *mut c_void,
    pub(crate) psvi: *mut c_void,
    pub(crate) parseFlags: c_int,
    pub(crate) properties: c_int,
}

/// `struct _xmlDtd`.
#[repr(C)]
pub(crate) struct xmlDtd {
    pub(crate) private: *mut c_void,
    pub(crate) type_: c_int,
    pub(crate) name: *const xmlChar,
    pub(crate) children: *mut xmlNode,
    pub(crate) last: *mut xmlNode,
    pub(crate) parent: *mut xmlDoc,
    pub(crate) next: *mut xmlNode,
    pub(crate) prev: *mut xmlNode,
    pub(crate) doc: *mut xmlDoc,
    pub(crate) notations: *mut c_void,
    pub(crate) elements: *mut c_void,
    pub(crate) attributes: *mut c_void,
    pub(crate) entities: *mut c_void,
    pub(crate) ExternalID: *const xmlChar,
    pub(crate) SystemID: *const xmlChar,
    pub(crate) pentities: *mut c_void,
}

/// `struct _xmlEntity`.
#[repr(C)]
pub(crate) struct xmlEntity {
    pub(crate) private: *mut c_void,
    pub(crate) type_: c_int,
    pub(crate) name: *const xmlChar,
    pub(crate) children: *mut xmlNode,
    pub(crate) last: *mut xmlNode,
    pub(crate) parent: *mut xmlDtd,
    pub(crate) next: *mut xmlNode,
    pub(crate) prev: *mut xmlNode,
    pub(crate) doc: *mut xmlDoc,
    pub(crate) orig: *mut xmlChar,
    pub(crate) content: *mut xmlChar,
    pub(crate) length: c_int,
    pub(crate) etype: c_int,
    pub(crate) ExternalID: *const xmlChar,
    pub(crate) SystemID: *const xmlChar,
    pub(crate) nexte: *mut xmlEntity,
    pub(crate) URI: *const xmlChar,
    pub(crate) owner: c_int,
    pub(crate) checked: c_int,
}

/// `struct _xmlError`.
#[repr(C)]
pub(crate) struct xmlError {
    pub(crate) domain: c_int,
    pub(crate) code: c_int,
    pub(crate) message: *mut c_char,
    pub(crate) level: c_int,
    pub(crate) file: *mut c_char,
    pub(crate) line: c_int,
    pub(crate) str1: *mut c_char,
    pub(crate) str2: *mut c_char,
    pub(crate) str3: *mut c_char,
    pub(crate) int1: c_int,
    pub(crate) int2: c_int,
    pub(crate) ctxt: *mut c_void,
    pub(crate) node: *mut c_void,
}

/// `struct _xmlValidCtxt`.
#[repr(C)]
pub(crate) struct xmlValidCtxt {
    pub(crate) userData: *mut c_void,
    pub(crate) error: *mut c_void,
    pub(crate) warning: *mut c_void,
    pub(crate) node: *mut xmlNode,
    pub(crate) nodeNr: c_int,
    pub(crate) nodeMax: c_int,
    pub(crate) nodeTab: *mut *mut xmlNode,
    pub(crate) finishDtd: c_uint,
    pub(crate) doc: *mut xmlDoc,
    pub(crate) valid: c_int,
    pub(crate) vstate: *mut c_void,
    pub(crate) vstateNr: c_int,
    pub(crate) vstateMax: c_int,
    pub(crate) vstateTab: *mut c_void,
    pub(crate) am: *mut c_void,
    pub(crate) state: *mut c_void,
}

/// `struct _xmlParserInput`, as far as `col`.
#[repr(C)]
pub(crate) struct xmlParserInput {
    pub(crate) buf: *mut c_void,
    pub(crate) filename: *const c_char,
    pub(crate) directory: *const c_char,
    pub(crate) base: *const xmlChar,
    pub(crate) cur: *const xmlChar,
    pub(crate) end: *const xmlChar,
    pub(crate) length: c_int,
    pub(crate) line: c_int,
    pub(crate) col: c_int,
}

/// `struct _xmlParserNodeInfoSeq`.
#[repr(C)]
pub(crate) struct xmlParserNodeInfoSeq {
    pub(crate) maximum: c_ulong,
    pub(crate) length: c_ulong,
    pub(crate) buffer: *mut c_void,
}

/// `struct _xmlParserCtxt`, as far as `options`: the fields php reads
/// and writes, the tail left out since this binding never lays one down.
#[repr(C)]
pub(crate) struct xmlParserCtxt {
    pub(crate) sax: *mut c_void,
    pub(crate) userData: *mut c_void,
    pub(crate) myDoc: *mut xmlDoc,
    pub(crate) wellFormed: c_int,
    pub(crate) replaceEntities: c_int,
    pub(crate) version: *const xmlChar,
    pub(crate) encoding: *const xmlChar,
    pub(crate) standalone: c_int,
    pub(crate) html: c_int,
    pub(crate) input: *mut xmlParserInput,
    pub(crate) inputNr: c_int,
    pub(crate) inputMax: c_int,
    pub(crate) inputTab: *mut *mut xmlParserInput,
    pub(crate) node: *mut xmlNode,
    pub(crate) nodeNr: c_int,
    pub(crate) nodeMax: c_int,
    pub(crate) nodeTab: *mut *mut xmlNode,
    pub(crate) record_info: c_int,
    pub(crate) node_seq: xmlParserNodeInfoSeq,
    pub(crate) errNo: c_int,
    pub(crate) hasExternalSubset: c_int,
    pub(crate) hasPErefs: c_int,
    pub(crate) external: c_int,
    pub(crate) valid: c_int,
    pub(crate) validate: c_int,
    pub(crate) vctxt: xmlValidCtxt,
    pub(crate) instate: c_int,
    pub(crate) token: c_int,
    pub(crate) directory: *mut c_char,
    pub(crate) name: *const xmlChar,
    pub(crate) nameNr: c_int,
    pub(crate) nameMax: c_int,
    pub(crate) nameTab: *mut *const xmlChar,
    pub(crate) nbChars: c_long,
    pub(crate) checkIndex: c_long,
    pub(crate) keepBlanks: c_int,
    pub(crate) disableSAX: c_int,
    pub(crate) inSubset: c_int,
    pub(crate) intSubName: *const xmlChar,
    pub(crate) extSubURI: *mut xmlChar,
    pub(crate) extSubSystem: *mut xmlChar,
    pub(crate) space: *mut c_int,
    pub(crate) spaceNr: c_int,
    pub(crate) spaceMax: c_int,
    pub(crate) spaceTab: *mut c_int,
    pub(crate) depth: c_int,
    pub(crate) entity: *mut xmlParserInput,
    pub(crate) charset: c_int,
    pub(crate) nodelen: c_int,
    pub(crate) nodemem: c_int,
    pub(crate) pedantic: c_int,
    pub(crate) private: *mut c_void,
    pub(crate) loadsubset: c_int,
    pub(crate) linenumbers: c_int,
    pub(crate) catalogs: *mut c_void,
    pub(crate) recovery: c_int,
    pub(crate) progressive: c_int,
    pub(crate) dict: *mut c_void,
    pub(crate) atts: *mut *const xmlChar,
    pub(crate) maxatts: c_int,
    pub(crate) docdict: c_int,
    pub(crate) str_xml: *const xmlChar,
    pub(crate) str_xmlns: *const xmlChar,
    pub(crate) str_xml_ns: *const xmlChar,
    pub(crate) sax2: c_int,
    pub(crate) nsNr: c_int,
    pub(crate) nsMax: c_int,
    pub(crate) nsTab: *mut *const xmlChar,
    pub(crate) attallocs: *mut c_int,
    pub(crate) pushTab: *mut c_void,
    pub(crate) attsDefault: *mut c_void,
    pub(crate) attsSpecial: *mut c_void,
    pub(crate) nsWellFormed: c_int,
    pub(crate) options: c_int,
}

/// `struct _xmlNodeSet`.
#[repr(C)]
pub(crate) struct xmlNodeSet {
    pub(crate) nodeNr: c_int,
    pub(crate) nodeMax: c_int,
    pub(crate) nodeTab: *mut *mut xmlNode,
}

/// `struct _xmlXPathObject`.
#[repr(C)]
pub(crate) struct xmlXPathObject {
    pub(crate) type_: c_int,
    pub(crate) nodesetval: *mut xmlNodeSet,
    pub(crate) boolval: c_int,
    pub(crate) floatval: c_double,
    pub(crate) stringval: *mut xmlChar,
    pub(crate) user: *mut c_void,
    pub(crate) index: c_int,
    pub(crate) user2: *mut c_void,
    pub(crate) index2: c_int,
}

/// `struct _xmlXPathContext`, as far as `userData`.
#[repr(C)]
pub(crate) struct xmlXPathContext {
    pub(crate) doc: *mut xmlDoc,
    pub(crate) node: *mut xmlNode,
    pub(crate) nb_variables_unused: c_int,
    pub(crate) max_variables_unused: c_int,
    pub(crate) varHash: *mut c_void,
    pub(crate) nb_types: c_int,
    pub(crate) max_types: c_int,
    pub(crate) types: *mut c_void,
    pub(crate) nb_funcs_unused: c_int,
    pub(crate) max_funcs_unused: c_int,
    pub(crate) funcHash: *mut c_void,
    pub(crate) nb_axis: c_int,
    pub(crate) max_axis: c_int,
    pub(crate) axis: *mut c_void,
    pub(crate) namespaces: *mut *mut xmlNs,
    pub(crate) nsNr: c_int,
    pub(crate) user: *mut c_void,
    pub(crate) contextSize: c_int,
    pub(crate) proximityPosition: c_int,
    pub(crate) xptr: c_int,
    pub(crate) here: *mut xmlNode,
    pub(crate) origin: *mut xmlNode,
    pub(crate) nsHash: *mut c_void,
    pub(crate) varLookupFunc: *mut c_void,
    pub(crate) varLookupData: *mut c_void,
    pub(crate) extra: *mut c_void,
    pub(crate) function: *const xmlChar,
    pub(crate) functionURI: *const xmlChar,
    pub(crate) funcLookupFunc: *mut c_void,
    pub(crate) funcLookupData: *mut c_void,
    pub(crate) tmpNsList: *mut *mut xmlNs,
    pub(crate) tmpNsNr: c_int,
    pub(crate) userData: *mut c_void,
}

/// `struct _xmlXPathParserContext`, as far as `valueTab`.
#[repr(C)]
pub(crate) struct xmlXPathParserContext {
    pub(crate) cur: *const xmlChar,
    pub(crate) base: *const xmlChar,
    pub(crate) error: c_int,
    pub(crate) context: *mut xmlXPathContext,
    pub(crate) value: *mut xmlXPathObject,
    pub(crate) valueNr: c_int,
    pub(crate) valueMax: c_int,
    pub(crate) valueTab: *mut *mut xmlXPathObject,
}

/// `xmlOutputBuffer`, never read through.
#[repr(C)]
pub(crate) struct xmlOutputBuffer {
    _opaque: [u8; 0],
}
/// `xmlSaveCtxt`, never read through.
#[repr(C)]
pub(crate) struct xmlSaveCtxt {
    _opaque: [u8; 0],
}
/// `xmlCharEncodingHandler`, never read through.
#[repr(C)]
pub(crate) struct xmlCharEncodingHandler {
    _opaque: [u8; 0],
}

pub(crate) type xmlStructuredErrorFunc =
    unsafe extern "C" fn(user: *mut c_void, error: *mut xmlError);
pub(crate) type xmlOutputWriteCallback =
    unsafe extern "C" fn(context: *mut c_void, buffer: *const c_char, len: c_int) -> c_int;
pub(crate) type xmlOutputCloseCallback = unsafe extern "C" fn(context: *mut c_void) -> c_int;
pub(crate) type xmlXPathFunction =
    unsafe extern "C" fn(ctxt: *mut xmlXPathParserContext, nargs: c_int);
pub(crate) type xmlHashScanner =
    unsafe extern "C" fn(payload: *mut c_void, data: *mut c_void, name: *const xmlChar);
pub(crate) type xmlFreeFunc = unsafe extern "C" fn(mem: *mut c_void);
pub(crate) type xmlC14NIsVisibleCallback =
    unsafe extern "C" fn(user_data: *mut c_void, node: *mut xmlNode, parent: *mut xmlNode) -> c_int;
pub(crate) type xmlMallocFunc = unsafe extern "C" fn(size: usize) -> *mut c_void;

#[link(name = "xml2")]
unsafe extern "C" {
    pub(crate) safe static xmlParserVersion: *const c_char;

    pub(crate) fn xmlInitParser();
    /// `xmlFree`: libxml2 is built without per-thread allocators, so the
    /// function is one process-wide variable, set before any thread starts.
    pub(crate) static xmlFree: xmlFreeFunc;
    /// `xmlMalloc`, the same.
    pub(crate) static xmlMalloc: xmlMallocFunc;
    pub(crate) fn xmlStrdup(cur: *const xmlChar) -> *mut xmlChar;
    pub(crate) fn xmlStrndup(cur: *const xmlChar, len: c_int) -> *mut xmlChar;
    pub(crate) fn xmlStrEqual(a: *const xmlChar, b: *const xmlChar) -> c_int;

    // xmlerror.h
    pub(crate) fn xmlSetStructuredErrorFunc(
        ctx: *mut c_void,
        handler: Option<xmlStructuredErrorFunc>,
    );
    pub(crate) fn __xmlStructuredError() -> *mut Option<xmlStructuredErrorFunc>;
    pub(crate) fn __xmlStructuredErrorContext() -> *mut *mut c_void;
    pub(crate) fn xmlGetLastError() -> *mut xmlError;
    pub(crate) fn xmlResetLastError();

    // tree.h
    pub(crate) fn xmlNewDoc(version: *const xmlChar) -> *mut xmlDoc;
    pub(crate) fn xmlFreeDoc(doc: *mut xmlDoc);
    pub(crate) fn xmlFreeNode(node: *mut xmlNode);
    pub(crate) fn xmlFreeNodeList(node: *mut xmlNode);
    pub(crate) fn xmlFreeProp(attr: *mut xmlAttr);
    pub(crate) fn xmlFreeNs(ns: *mut xmlNs);
    pub(crate) fn xmlFreeDtd(dtd: *mut xmlDtd);
    pub(crate) fn xmlUnlinkNode(node: *mut xmlNode);
    pub(crate) fn xmlAddChild(parent: *mut xmlNode, child: *mut xmlNode) -> *mut xmlNode;
    pub(crate) fn xmlAddChildList(parent: *mut xmlNode, child: *mut xmlNode) -> *mut xmlNode;
    pub(crate) fn xmlAddPrevSibling(cur: *mut xmlNode, elem: *mut xmlNode) -> *mut xmlNode;
    pub(crate) fn xmlAddNextSibling(cur: *mut xmlNode, elem: *mut xmlNode) -> *mut xmlNode;
    pub(crate) fn xmlReplaceNode(old: *mut xmlNode, cur: *mut xmlNode) -> *mut xmlNode;
    pub(crate) fn xmlDocGetRootElement(doc: *const xmlDoc) -> *mut xmlNode;
    pub(crate) fn xmlDocSetRootElement(doc: *mut xmlDoc, root: *mut xmlNode) -> *mut xmlNode;
    pub(crate) fn xmlNewDocNode(
        doc: *mut xmlDoc,
        ns: *mut xmlNs,
        name: *const xmlChar,
        content: *const xmlChar,
    ) -> *mut xmlNode;
    pub(crate) fn xmlNewDocRawNode(
        doc: *mut xmlDoc,
        ns: *mut xmlNs,
        name: *const xmlChar,
        content: *const xmlChar,
    ) -> *mut xmlNode;
    pub(crate) fn xmlNewNode(ns: *mut xmlNs, name: *const xmlChar) -> *mut xmlNode;
    pub(crate) fn xmlNewDocText(doc: *const xmlDoc, content: *const xmlChar) -> *mut xmlNode;
    pub(crate) fn xmlNewDocTextLen(
        doc: *mut xmlDoc,
        content: *const xmlChar,
        len: c_int,
    ) -> *mut xmlNode;
    pub(crate) fn xmlNewTextLen(content: *const xmlChar, len: c_int) -> *mut xmlNode;
    pub(crate) fn xmlNewDocComment(doc: *mut xmlDoc, content: *const xmlChar) -> *mut xmlNode;
    pub(crate) fn xmlNewCDataBlock(
        doc: *mut xmlDoc,
        content: *const xmlChar,
        len: c_int,
    ) -> *mut xmlNode;
    pub(crate) fn xmlNewDocPI(
        doc: *mut xmlDoc,
        name: *const xmlChar,
        content: *const xmlChar,
    ) -> *mut xmlNode;
    pub(crate) fn xmlNewReference(doc: *const xmlDoc, name: *const xmlChar) -> *mut xmlNode;
    pub(crate) fn xmlNewDocFragment(doc: *mut xmlDoc) -> *mut xmlNode;
    pub(crate) fn xmlNewDocProp(
        doc: *mut xmlDoc,
        name: *const xmlChar,
        value: *const xmlChar,
    ) -> *mut xmlAttr;
    pub(crate) fn xmlNewNs(
        node: *mut xmlNode,
        href: *const xmlChar,
        prefix: *const xmlChar,
    ) -> *mut xmlNs;
    pub(crate) fn xmlSetNs(node: *mut xmlNode, ns: *mut xmlNs);
    pub(crate) fn xmlSearchNs(
        doc: *mut xmlDoc,
        node: *mut xmlNode,
        prefix: *const xmlChar,
    ) -> *mut xmlNs;
    pub(crate) fn xmlSearchNsByHref(
        doc: *mut xmlDoc,
        node: *mut xmlNode,
        href: *const xmlChar,
    ) -> *mut xmlNs;
    pub(crate) fn xmlReconciliateNs(doc: *mut xmlDoc, tree: *mut xmlNode) -> c_int;
    pub(crate) fn xmlSetTreeDoc(tree: *mut xmlNode, doc: *mut xmlDoc);
    pub(crate) fn xmlDocCopyNode(
        node: *mut xmlNode,
        doc: *mut xmlDoc,
        recursive: c_int,
    ) -> *mut xmlNode;
    pub(crate) fn xmlCopyDtd(dtd: *mut xmlDtd) -> *mut xmlDtd;
    pub(crate) fn xmlNodeGetContent(node: *const xmlNode) -> *mut xmlChar;
    pub(crate) fn xmlNodeSetContent(node: *mut xmlNode, content: *const xmlChar);
    pub(crate) fn xmlNodeSetContentLen(node: *mut xmlNode, content: *const xmlChar, len: c_int);
    pub(crate) fn xmlNodeAddContent(node: *mut xmlNode, content: *const xmlChar);
    pub(crate) fn xmlNodeAddContentLen(node: *mut xmlNode, content: *const xmlChar, len: c_int);
    pub(crate) fn xmlNodeListGetString(
        doc: *mut xmlDoc,
        list: *const xmlNode,
        inline: c_int,
    ) -> *mut xmlChar;
    pub(crate) fn xmlNodeGetBase(doc: *const xmlDoc, node: *const xmlNode) -> *mut xmlChar;
    pub(crate) fn xmlGetNodePath(node: *const xmlNode) -> *mut xmlChar;
    pub(crate) fn xmlGetLineNo(node: *const xmlNode) -> c_long;
    pub(crate) fn xmlTextConcat(node: *mut xmlNode, content: *const xmlChar, len: c_int) -> c_int;
    pub(crate) fn xmlHasProp(node: *const xmlNode, name: *const xmlChar) -> *mut xmlAttr;
    pub(crate) fn xmlHasNsProp(
        node: *const xmlNode,
        name: *const xmlChar,
        ns: *const xmlChar,
    ) -> *mut xmlAttr;
    pub(crate) fn xmlGetNsProp(
        node: *const xmlNode,
        name: *const xmlChar,
        ns: *const xmlChar,
    ) -> *mut xmlChar;
    pub(crate) fn xmlGetNoNsProp(node: *const xmlNode, name: *const xmlChar) -> *mut xmlChar;
    pub(crate) fn xmlSetProp(
        node: *mut xmlNode,
        name: *const xmlChar,
        value: *const xmlChar,
    ) -> *mut xmlAttr;
    pub(crate) fn xmlSetNsProp(
        node: *mut xmlNode,
        ns: *mut xmlNs,
        name: *const xmlChar,
        value: *const xmlChar,
    ) -> *mut xmlAttr;
    pub(crate) fn xmlSplitQName2(name: *const xmlChar, prefix: *mut *mut xmlChar) -> *mut xmlChar;
    pub(crate) fn xmlValidateQName(value: *const xmlChar, space: c_int) -> c_int;
    pub(crate) fn xmlValidateName(value: *const xmlChar, space: c_int) -> c_int;
    pub(crate) fn xmlValidateNCName(value: *const xmlChar, space: c_int) -> c_int;
    pub(crate) fn xmlCreateIntSubset(
        doc: *mut xmlDoc,
        name: *const xmlChar,
        external_id: *const xmlChar,
        system_id: *const xmlChar,
    ) -> *mut xmlDtd;
    pub(crate) fn xmlGetIntSubset(doc: *const xmlDoc) -> *mut xmlDtd;
    pub(crate) fn xmlIsBlankNode(node: *const xmlNode) -> c_int;
    pub(crate) fn xmlDOMWrapAdoptNode(
        ctxt: *mut c_void,
        source: *mut xmlDoc,
        node: *mut xmlNode,
        dest: *mut xmlDoc,
        dest_parent: *mut xmlNode,
        options: c_int,
    ) -> c_int;
    pub(crate) fn xmlUTF8Strlen(utf: *const xmlChar) -> c_int;
    pub(crate) fn xmlUTF8Strsize(utf: *const xmlChar, len: c_int) -> c_int;
    pub(crate) fn xmlUTF8Strpos(utf: *const xmlChar, pos: c_int) -> *const xmlChar;
    pub(crate) fn xmlUTF8Strsub(utf: *const xmlChar, start: c_int, len: c_int) -> *mut xmlChar;

    // valid.h, entities.h, hash.h, dict.h
    pub(crate) fn xmlGetID(doc: *mut xmlDoc, id: *const xmlChar) -> *mut xmlAttr;
    pub(crate) fn xmlAddID(
        ctxt: *mut c_void,
        doc: *mut xmlDoc,
        value: *const xmlChar,
        attr: *mut xmlAttr,
    ) -> *mut c_void;
    pub(crate) fn xmlRemoveID(doc: *mut xmlDoc, attr: *mut xmlAttr) -> c_int;
    pub(crate) fn xmlNewValidCtxt() -> *mut xmlValidCtxt;
    pub(crate) fn xmlFreeValidCtxt(ctxt: *mut xmlValidCtxt);
    pub(crate) fn xmlValidateDocument(ctxt: *mut xmlValidCtxt, doc: *mut xmlDoc) -> c_int;
    pub(crate) fn xmlGetDocEntity(doc: *const xmlDoc, name: *const xmlChar) -> *mut xmlEntity;
    pub(crate) fn xmlHashLookup(table: *mut c_void, name: *const xmlChar) -> *mut c_void;
    pub(crate) fn xmlHashSize(table: *mut c_void) -> c_int;
    pub(crate) fn xmlHashScan(table: *mut c_void, f: xmlHashScanner, data: *mut c_void);
    pub(crate) fn xmlHashRemoveEntry(
        table: *mut c_void,
        name: *const xmlChar,
        dealloc: *mut c_void,
    ) -> c_int;
    pub(crate) fn xmlDictOwns(dict: *mut c_void, s: *const xmlChar) -> c_int;

    // parser.h, parserInternals.h
    pub(crate) fn xmlCreateMemoryParserCtxt(
        buffer: *const c_char,
        size: c_int,
    ) -> *mut xmlParserCtxt;
    pub(crate) fn xmlCreateFileParserCtxt(filename: *const c_char) -> *mut xmlParserCtxt;
    pub(crate) fn xmlFreeParserCtxt(ctxt: *mut xmlParserCtxt);
    pub(crate) fn xmlCtxtUseOptions(ctxt: *mut xmlParserCtxt, options: c_int) -> c_int;
    pub(crate) fn xmlParseDocument(ctxt: *mut xmlParserCtxt) -> c_int;
    pub(crate) fn xmlCanonicPath(path: *const xmlChar) -> *mut xmlChar;
    pub(crate) fn xmlParseBalancedChunkMemory(
        doc: *mut xmlDoc,
        sax: *mut c_void,
        user_data: *mut c_void,
        depth: c_int,
        string: *const xmlChar,
        list: *mut *mut xmlNode,
    ) -> c_int;
    pub(crate) fn xmlSwitchToEncoding(
        ctxt: *mut xmlParserCtxt,
        handler: *mut xmlCharEncodingHandler,
    ) -> c_int;
    pub(crate) fn xmlFindCharEncodingHandler(name: *const c_char) -> *mut xmlCharEncodingHandler;
    pub(crate) fn xmlCharEncCloseFunc(handler: *mut xmlCharEncodingHandler) -> c_int;
    pub(crate) fn xmlXIncludeProcessFlags(doc: *mut xmlDoc, flags: c_int) -> c_int;

    // HTMLparser.h, HTMLtree.h
    pub(crate) fn htmlCreateMemoryParserCtxt(
        buffer: *const c_char,
        size: c_int,
    ) -> *mut xmlParserCtxt;
    pub(crate) fn htmlCreateFileParserCtxt(
        filename: *const c_char,
        encoding: *const c_char,
    ) -> *mut xmlParserCtxt;
    pub(crate) fn htmlCtxtUseOptions(ctxt: *mut xmlParserCtxt, options: c_int) -> c_int;
    pub(crate) fn htmlParseDocument(ctxt: *mut xmlParserCtxt) -> c_int;
    pub(crate) fn htmlFreeParserCtxt(ctxt: *mut xmlParserCtxt);
    pub(crate) fn htmlNewDocNoDtD(uri: *const xmlChar, external_id: *const xmlChar) -> *mut xmlDoc;
    pub(crate) fn htmlNodeDumpFormatOutput(
        buf: *mut xmlOutputBuffer,
        doc: *mut xmlDoc,
        node: *mut xmlNode,
        encoding: *const c_char,
        format: c_int,
    );
    pub(crate) fn htmlDocDumpMemoryFormat(
        doc: *mut xmlDoc,
        mem: *mut *mut xmlChar,
        size: *mut c_int,
        format: c_int,
    );
    pub(crate) fn htmlSaveFileFormat(
        filename: *const c_char,
        doc: *mut xmlDoc,
        encoding: *const c_char,
        format: c_int,
    ) -> c_int;

    // xmlsave.h, xmlIO.h
    pub(crate) fn xmlSaveToIO(
        iowrite: xmlOutputWriteCallback,
        ioclose: Option<xmlOutputCloseCallback>,
        ioctx: *mut c_void,
        encoding: *const c_char,
        options: c_int,
    ) -> *mut xmlSaveCtxt;
    pub(crate) fn xmlSaveDoc(ctxt: *mut xmlSaveCtxt, doc: *mut xmlDoc) -> c_long;
    pub(crate) fn xmlSaveClose(ctxt: *mut xmlSaveCtxt) -> c_int;
    pub(crate) fn xmlSaveFormatFileEnc(
        filename: *const c_char,
        doc: *mut xmlDoc,
        encoding: *const c_char,
        format: c_int,
    ) -> c_int;
    pub(crate) fn xmlOutputBufferCreateIO(
        iowrite: xmlOutputWriteCallback,
        ioclose: Option<xmlOutputCloseCallback>,
        ioctx: *mut c_void,
        encoder: *mut xmlCharEncodingHandler,
    ) -> *mut xmlOutputBuffer;
    pub(crate) fn xmlOutputBufferCreateFilename(
        uri: *const c_char,
        encoder: *mut xmlCharEncodingHandler,
        compression: c_int,
    ) -> *mut xmlOutputBuffer;
    pub(crate) fn xmlOutputBufferFlush(out: *mut xmlOutputBuffer) -> c_int;
    pub(crate) fn xmlOutputBufferClose(out: *mut xmlOutputBuffer) -> c_int;
    pub(crate) fn xmlNodeDumpOutput(
        buf: *mut xmlOutputBuffer,
        doc: *mut xmlDoc,
        node: *mut xmlNode,
        level: c_int,
        format: c_int,
        encoding: *const c_char,
    );

    // c14n.h
    pub(crate) fn xmlC14NDocSaveTo(
        doc: *mut xmlDoc,
        nodes: *mut xmlNodeSet,
        mode: c_int,
        inclusive_ns_prefixes: *mut *mut xmlChar,
        with_comments: c_int,
        buf: *mut xmlOutputBuffer,
    ) -> c_int;

    // xpath.h, xpathInternals.h
    pub(crate) fn xmlXPathNewContext(doc: *mut xmlDoc) -> *mut xmlXPathContext;
    pub(crate) fn xmlXPathFreeContext(ctxt: *mut xmlXPathContext);
    pub(crate) fn xmlXPathEval(
        expr: *const xmlChar,
        ctxt: *mut xmlXPathContext,
    ) -> *mut xmlXPathObject;
    pub(crate) fn xmlXPathEvalExpression(
        expr: *const xmlChar,
        ctxt: *mut xmlXPathContext,
    ) -> *mut xmlXPathObject;
    pub(crate) fn xmlXPathFreeObject(obj: *mut xmlXPathObject);
    pub(crate) fn xmlXPathRegisterNs(
        ctxt: *mut xmlXPathContext,
        prefix: *const xmlChar,
        uri: *const xmlChar,
    ) -> c_int;
    pub(crate) fn xmlXPathRegisterFuncNS(
        ctxt: *mut xmlXPathContext,
        name: *const xmlChar,
        uri: *const xmlChar,
        f: Option<xmlXPathFunction>,
    ) -> c_int;
    pub(crate) fn valuePop(ctxt: *mut xmlXPathParserContext) -> *mut xmlXPathObject;
    pub(crate) fn valuePush(ctxt: *mut xmlXPathParserContext, value: *mut xmlXPathObject) -> c_int;
    pub(crate) fn xmlXPathNewString(val: *const xmlChar) -> *mut xmlXPathObject;
    pub(crate) fn xmlXPathNewFloat(val: c_double) -> *mut xmlXPathObject;
    pub(crate) fn xmlXPathNewBoolean(val: c_int) -> *mut xmlXPathObject;
    pub(crate) fn xmlXPathNewNodeSet(node: *mut xmlNode) -> *mut xmlXPathObject;
    pub(crate) fn xmlXPathNodeSetAdd(set: *mut xmlNodeSet, node: *mut xmlNode) -> c_int;
    pub(crate) fn xmlXPathCastToString(val: *mut xmlXPathObject) -> *mut xmlChar;
    pub(crate) fn xmlXPathErr(ctxt: *mut xmlXPathParserContext, error: c_int);
    pub(crate) fn xmlXPathNodeSetCreate(node: *mut xmlNode) -> *mut xmlNodeSet;
    pub(crate) fn xmlXPathFreeNodeSet(set: *mut xmlNodeSet);

    // uri.h
    pub(crate) fn xmlBuildURI(uri: *const xmlChar, base: *const xmlChar) -> *mut xmlChar;

    // tree.h, xmlstring.h, globals.h: what php's dom reaches beside the above
    pub(crate) fn xmlNewProp(
        node: *mut xmlNode,
        name: *const xmlChar,
        value: *const xmlChar,
    ) -> *mut xmlAttr;
    pub(crate) fn xmlNewPI(name: *const xmlChar, content: *const xmlChar) -> *mut xmlNode;
    pub(crate) fn xmlNewComment(content: *const xmlChar) -> *mut xmlNode;
    pub(crate) fn xmlNewText(content: *const xmlChar) -> *mut xmlNode;
    pub(crate) fn xmlSplitQName3(name: *const xmlChar, len: *mut c_int) -> *const xmlChar;
    pub(crate) fn xmlUTF8Strndup(utf: *const xmlChar, len: c_int) -> *mut xmlChar;
    pub(crate) fn __xmlSaveNoEmptyTags() -> *mut c_int;
    pub(crate) fn xmlC14NExecute(
        doc: *mut xmlDoc,
        is_visible: Option<xmlC14NIsVisibleCallback>,
        user_data: *mut c_void,
        mode: c_int,
        inclusive_ns_prefixes: *mut *mut xmlChar,
        with_comments: c_int,
        buf: *mut xmlOutputBuffer,
    ) -> c_int;

    pub(crate) fn xmlDictLookup(
        dict: *mut c_void,
        name: *const xmlChar,
        len: c_int,
    ) -> *const xmlChar;
    pub(crate) fn xmlGetNsList(doc: *const xmlDoc, node: *const xmlNode) -> *mut *mut xmlNs;
    pub(crate) fn htmlGetMetaEncoding(doc: *mut xmlDoc) -> *const xmlChar;

    // xmlschemas.h, relaxng.h
    pub(crate) fn xmlSchemaNewParserCtxt(url: *const c_char) -> *mut c_void;
    pub(crate) fn xmlSchemaNewMemParserCtxt(buffer: *const c_char, size: c_int) -> *mut c_void;
    pub(crate) fn xmlSchemaParse(ctxt: *mut c_void) -> *mut c_void;
    pub(crate) fn xmlSchemaFreeParserCtxt(ctxt: *mut c_void);
    pub(crate) fn xmlSchemaNewValidCtxt(schema: *mut c_void) -> *mut c_void;
    pub(crate) fn xmlSchemaSetValidOptions(ctxt: *mut c_void, options: c_int) -> c_int;
    pub(crate) fn xmlSchemaValidateDoc(ctxt: *mut c_void, doc: *mut xmlDoc) -> c_int;
    pub(crate) fn xmlSchemaFree(schema: *mut c_void);
    pub(crate) fn xmlSchemaFreeValidCtxt(ctxt: *mut c_void);
    pub(crate) fn xmlRelaxNGNewParserCtxt(url: *const c_char) -> *mut c_void;
    pub(crate) fn xmlRelaxNGNewMemParserCtxt(buffer: *const c_char, size: c_int) -> *mut c_void;
    pub(crate) fn xmlRelaxNGParse(ctxt: *mut c_void) -> *mut c_void;
    pub(crate) fn xmlRelaxNGFreeParserCtxt(ctxt: *mut c_void);
    pub(crate) fn xmlRelaxNGNewValidCtxt(schema: *mut c_void) -> *mut c_void;
    pub(crate) fn xmlRelaxNGValidateDoc(ctxt: *mut c_void, doc: *mut xmlDoc) -> c_int;
    pub(crate) fn xmlRelaxNGFree(schema: *mut c_void);
    pub(crate) fn xmlRelaxNGFreeValidCtxt(ctxt: *mut c_void);
}

#[cfg(test)]
mod tests {
    use std::mem::{offset_of, size_of};

    use super::*;

    /// The offsets and sizes `offsetof` and `sizeof` read off the
    /// platform's headers at 2.9.13, so a field out of place is a red test
    /// and not a wrong read.
    #[test]
    fn the_layout_is_the_headers() {
        assert_eq!(size_of::<xmlNode>(), 120);
        assert_eq!(offset_of!(xmlNode, ns), 72);
        assert_eq!(offset_of!(xmlNode, line), 112);
        assert_eq!(size_of::<xmlAttr>(), 96);
        assert_eq!(offset_of!(xmlAttr, atype), 80);
        assert_eq!(size_of::<xmlDoc>(), 176);
        assert_eq!(offset_of!(xmlDoc, intSubset), 80);
        assert_eq!(offset_of!(xmlDoc, URL), 136);
        assert_eq!(offset_of!(xmlDoc, properties), 172);
        assert_eq!(size_of::<xmlNs>(), 48);
        assert_eq!(size_of::<xmlDtd>(), 128);
        assert_eq!(size_of::<xmlEntity>(), 136);
        assert_eq!(offset_of!(xmlEntity, etype), 92);
        assert_eq!(size_of::<xmlError>(), 88);
        assert_eq!(offset_of!(xmlError, int2), 68);
        assert_eq!(size_of::<xmlValidCtxt>(), 112);
        assert_eq!(offset_of!(xmlParserInput, line), 52);
        assert_eq!(offset_of!(xmlParserInput, col), 56);
        assert_eq!(offset_of!(xmlParserCtxt, input), 56);
        assert_eq!(offset_of!(xmlParserCtxt, vctxt), 160);
        assert_eq!(offset_of!(xmlParserCtxt, directory), 280);
        assert_eq!(offset_of!(xmlParserCtxt, keepBlanks), 328);
        assert_eq!(offset_of!(xmlParserCtxt, private), 424);
        assert_eq!(offset_of!(xmlParserCtxt, recovery), 448);
        assert_eq!(offset_of!(xmlParserCtxt, options), 564);
        assert_eq!(size_of::<xmlXPathObject>(), 72);
        assert_eq!(offset_of!(xmlXPathObject, stringval), 32);
        assert_eq!(size_of::<xmlNodeSet>(), 16);
        assert_eq!(offset_of!(xmlXPathContext, namespaces), 80);
        assert_eq!(offset_of!(xmlXPathContext, nsNr), 88);
        assert_eq!(offset_of!(xmlXPathContext, userData), 216);
        assert_eq!(offset_of!(xmlXPathParserContext, context), 24);
        assert_eq!(offset_of!(xmlXPathParserContext, valueNr), 40);
    }
}
