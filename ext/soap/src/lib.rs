//! The `soap` extension: `php-src/ext/soap` as one crate under `ext/`
//! (ADR-0004), php's own C ported over `ext/libxml`'s surface with no
//! library of its own (design §7.2, revision 72), behind the `soap`
//! feature as php builds it `--enable-soap`: `soap.c`'s classes and
//! natives, `php_encoding.c`'s encoders, `php_sdl.c` and `php_schema.c`'s
//! WSDL and schema reading, `php_http.c`'s HTTP client and
//! `php_packet_soap.c`'s response parse. No `src/ffi/`, no `unsafe`. The phpt subset is
//! `php-src/ext/soap/tests`, through `xtask phpt --subset`.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]
// php's field names (`binding_type`, `param_name`, `function_name`) kept.
#![allow(clippy::struct_field_names)]
// php's signatures, kept: `serialize_parameter` and its kin take what the C
// takes, and its bindings (`ns`, `nsptr`) keep their names.
#![allow(clippy::too_many_arguments, clippy::similar_names)]

mod classes;
mod client;
mod encoding;
mod globals;
mod http;
mod message;
mod model;
mod packet;
mod schema;
mod sdl;
mod server;
mod shared;
mod table;
mod xml;

use php_ext::abi::{
    Dependency, DependencyKind, Extension, ExtensionBuilder, IniStage, LongHandler,
};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Value, ValueRef};

pub(crate) use globals::{Error, Res, TypemapEntry, raised, soap_error};
pub(crate) use globals::{State, Typemap, TypemapView};

/// Core's `Exception`, which `SoapFault` extends.
#[php_class(extern)]
pub(crate) struct Exception;

pub(crate) const SOAP_1_1: i64 = 1;
pub(crate) const SOAP_1_2: i64 = 2;

/// The constants `register_soap_symbols` registers, in the stub's order.
///
/// ```php
/// var_dump(SOAP_1_2, SOAP_ENC_ARRAY, XSD_ANYXML, XSD_NAMESPACE, WSDL_CACHE_BOTH, SOAP_SSL_METHOD_SSLv23);
/// var_dump((new ReflectionConstant('SOAP_FUNCTIONS_ALL'))->isDeprecated());
/// ```
/// ```expect
/// int(2)
/// int(300)
/// int(147)
/// string(32) "http://www.w3.org/2001/XMLSchema"
/// int(3)
/// int(3)
/// bool(true)
/// ```
#[php_const]
#[allow(non_upper_case_globals)]
pub(crate) mod constants {
    pub(crate) const SOAP_1_1: i64 = 1;
    pub(crate) const SOAP_1_2: i64 = 2;
    pub(crate) const SOAP_PERSISTENCE_SESSION: i64 = 1;
    pub(crate) const SOAP_PERSISTENCE_REQUEST: i64 = 2;
    #[php(deprecated(
        since = "8.4",
        message = "as enabling all functions is a possible security concern"
    ))]
    pub(crate) const SOAP_FUNCTIONS_ALL: i64 = 999;
    pub(crate) const SOAP_ENCODED: i64 = 1;
    pub(crate) const SOAP_LITERAL: i64 = 2;
    pub(crate) const SOAP_RPC: i64 = 1;
    pub(crate) const SOAP_DOCUMENT: i64 = 2;
    pub(crate) const SOAP_ACTOR_NEXT: i64 = 1;
    pub(crate) const SOAP_ACTOR_NONE: i64 = 2;
    pub(crate) const SOAP_ACTOR_UNLIMATERECEIVER: i64 = 3;
    pub(crate) const SOAP_COMPRESSION_ACCEPT: i64 = 32;
    pub(crate) const SOAP_COMPRESSION_GZIP: i64 = 0;
    pub(crate) const SOAP_COMPRESSION_DEFLATE: i64 = 16;
    pub(crate) const SOAP_AUTHENTICATION_BASIC: i64 = 0;
    pub(crate) const SOAP_AUTHENTICATION_DIGEST: i64 = 1;
    pub(crate) const UNKNOWN_TYPE: i64 = 999_998;
    pub(crate) const XSD_STRING: i64 = 101;
    pub(crate) const XSD_BOOLEAN: i64 = 102;
    pub(crate) const XSD_DECIMAL: i64 = 103;
    pub(crate) const XSD_FLOAT: i64 = 104;
    pub(crate) const XSD_DOUBLE: i64 = 105;
    pub(crate) const XSD_DURATION: i64 = 106;
    pub(crate) const XSD_DATETIME: i64 = 107;
    pub(crate) const XSD_TIME: i64 = 108;
    pub(crate) const XSD_DATE: i64 = 109;
    pub(crate) const XSD_GYEARMONTH: i64 = 110;
    pub(crate) const XSD_GYEAR: i64 = 111;
    pub(crate) const XSD_GMONTHDAY: i64 = 112;
    pub(crate) const XSD_GDAY: i64 = 113;
    pub(crate) const XSD_GMONTH: i64 = 114;
    pub(crate) const XSD_HEXBINARY: i64 = 115;
    pub(crate) const XSD_BASE64BINARY: i64 = 116;
    pub(crate) const XSD_ANYURI: i64 = 117;
    pub(crate) const XSD_QNAME: i64 = 118;
    pub(crate) const XSD_NOTATION: i64 = 119;
    pub(crate) const XSD_NORMALIZEDSTRING: i64 = 120;
    pub(crate) const XSD_TOKEN: i64 = 121;
    pub(crate) const XSD_LANGUAGE: i64 = 122;
    pub(crate) const XSD_NMTOKEN: i64 = 123;
    pub(crate) const XSD_NAME: i64 = 124;
    pub(crate) const XSD_NCNAME: i64 = 125;
    pub(crate) const XSD_ID: i64 = 126;
    pub(crate) const XSD_IDREF: i64 = 127;
    pub(crate) const XSD_IDREFS: i64 = 128;
    pub(crate) const XSD_ENTITY: i64 = 129;
    pub(crate) const XSD_ENTITIES: i64 = 130;
    pub(crate) const XSD_INTEGER: i64 = 131;
    pub(crate) const XSD_NONPOSITIVEINTEGER: i64 = 132;
    pub(crate) const XSD_NEGATIVEINTEGER: i64 = 133;
    pub(crate) const XSD_LONG: i64 = 134;
    pub(crate) const XSD_INT: i64 = 135;
    pub(crate) const XSD_SHORT: i64 = 136;
    pub(crate) const XSD_BYTE: i64 = 137;
    pub(crate) const XSD_NONNEGATIVEINTEGER: i64 = 138;
    pub(crate) const XSD_UNSIGNEDLONG: i64 = 139;
    pub(crate) const XSD_UNSIGNEDINT: i64 = 140;
    pub(crate) const XSD_UNSIGNEDSHORT: i64 = 141;
    pub(crate) const XSD_UNSIGNEDBYTE: i64 = 142;
    pub(crate) const XSD_POSITIVEINTEGER: i64 = 143;
    pub(crate) const XSD_NMTOKENS: i64 = 144;
    pub(crate) const XSD_ANYTYPE: i64 = 145;
    pub(crate) const XSD_ANYXML: i64 = 147;
    pub(crate) const APACHE_MAP: i64 = 200;
    pub(crate) const SOAP_ENC_OBJECT: i64 = 301;
    pub(crate) const SOAP_ENC_ARRAY: i64 = 300;
    pub(crate) const XSD_1999_TIMEINSTANT: i64 = 401;
    pub(crate) const XSD_NAMESPACE: &str = "http://www.w3.org/2001/XMLSchema";
    pub(crate) const XSD_1999_NAMESPACE: &str = "http://www.w3.org/1999/XMLSchema";
    pub(crate) const SOAP_SINGLE_ELEMENT_ARRAYS: i64 = 1;
    pub(crate) const SOAP_WAIT_ONE_WAY_CALLS: i64 = 2;
    pub(crate) const SOAP_USE_XSI_ARRAY_TYPE: i64 = 4;
    pub(crate) const WSDL_CACHE_NONE: i64 = 0;
    pub(crate) const WSDL_CACHE_DISK: i64 = 1;
    pub(crate) const WSDL_CACHE_MEMORY: i64 = 2;
    pub(crate) const WSDL_CACHE_BOTH: i64 = 3;
    pub(crate) const SOAP_SSL_METHOD_TLS: i64 = 0;
    pub(crate) const SOAP_SSL_METHOD_SSLv2: i64 = 1;
    pub(crate) const SOAP_SSL_METHOD_SSLv3: i64 = 2;
    pub(crate) const SOAP_SSL_METHOD_SSLv23: i64 = 3;
}

/// The ini entries, `PHP_INI_BEGIN()` in `soap.c`.
pub(crate) mod ini {
    pub(crate) const CACHE_ENABLED: &[u8] = b"soap.wsdl_cache_enabled";
    pub(crate) const CACHE_DIR: &[u8] = b"soap.wsdl_cache_dir";
    pub(crate) const CACHE_TTL: &[u8] = b"soap.wsdl_cache_ttl";
    pub(crate) const CACHE: &[u8] = b"soap.wsdl_cache";
    pub(crate) const CACHE_LIMIT: &[u8] = b"soap.wsdl_cache_limit";
}

fn ini_entry(name: &[u8], default: &[u8]) -> IniDecl {
    IniDecl {
        name: Name(name.into()),
        default: Some(default.into()),
        user: true,
        per_dir: true,
        system: true,
    }
}

/// The extension value (design §7.1): the two functions, the eight
/// classes in `PHP_MINIT_FUNCTION(soap)`'s order, the constants, the ini
/// entries and php's `ZEND_MOD_REQUIRED("date")`,
/// `ZEND_MOD_REQUIRED("libxml")` and `ZEND_MOD_OPTIONAL("session")`.
///
/// ```php
/// var_dump(extension_loaded('soap'), phpversion('soap') === PHP_VERSION);
/// print_r((new ReflectionExtension('soap'))->getDependencies());
/// var_dump(get_parent_class('SoapFault'), (new ReflectionClass('Soap\\Sdl'))->isFinal());
/// var_dump(ini_get('soap.wsdl_cache_dir'), ini_get('soap.wsdl_cache_limit'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// Array
/// (
///     [date] => Required
///     [libxml] => Required
///     [session] => Optional
/// )
/// string(9) "Exception"
/// bool(true)
/// string(4) "/tmp"
/// string(1) "5"
/// ```
///
/// `soap.wsdl_cache_ttl` and `soap.wsdl_cache_limit` are `OnUpdateLong`'s,
/// read by the quantity rule with its warning at startup, at `ini_set()`
/// and as the request ends:
///
/// ```php
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'soap.wsdl_cache_ttl=1Dx', '-r', 'var_dump(ini_set("soap.wsdl_cache_limit", "0x"), ini_set("soap.wsdl_cache_ttl", "60"));'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]), proc_close($p), "\n";
/// ```
/// ```expect process
/// Warning: Invalid "soap.wsdl_cache_ttl" setting. Invalid quantity "1Dx": unknown multiplier "x", interpreting as "1" for backwards compatibility in Unknown on line 0
///
/// Warning: Invalid "soap.wsdl_cache_limit" setting. Invalid quantity "0x": no digits after base prefix, interpreting as "0" for backwards compatibility in Command line code on line 1
/// string(1) "5"
/// string(3) "1Dx"
///
/// Warning: Invalid "soap.wsdl_cache_ttl" setting. Invalid quantity "1Dx": unknown multiplier "x", interpreting as "1" for backwards compatibility in Unknown on line 0
/// 0
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            use_soap_error_handler::declared(),
            is_soap_fault::declared(),
        ])
        .class::<client::SoapClient>()
        .class::<classes::SoapVar>()
        .class::<server::SoapServer>()
        .class::<classes::SoapFault>()
        .class::<classes::SoapParam>()
        .class::<classes::SoapHeader>()
        .class::<classes::Url>()
        .class::<classes::Sdl>()
        .constants(constants::declared())
        .ini(vec![
            ini_entry(ini::CACHE_ENABLED, b"1"),
            ini_entry(ini::CACHE_DIR, b"/tmp"),
            ini_entry(ini::CACHE_TTL, b"86400"),
            ini_entry(ini::CACHE, b"1"),
            ini_entry(ini::CACHE_LIMIT, b"5"),
        ])
        .on_modify(ini::CACHE_DIR, on_update_cache_dir)
        .on_update_long(ini::CACHE_TTL, LongHandler::Any)
        .on_update_long(ini::CACHE_LIMIT, LongHandler::Any)
        .dependencies(vec![
            dependency("date", DependencyKind::Required),
            dependency("libxml", DependencyKind::Required),
            dependency("session", DependencyKind::Optional),
        ])
        .request_state::<State>()
        .error(server::soap_error_handler)
        .info(soap_info)
        .build("soap")
}

fn dependency(name: &str, kind: DependencyKind) -> Dependency {
    Dependency {
        name: String::from(name),
        kind,
        rel: None,
        version: None,
    }
}

/// `OnUpdateCacheDir`: a value with a NUL refused; at runtime the
/// directory after any `n;mode;` prefix held to `open_basedir`.
///
/// ```php
/// var_dump(ini_set('soap.wsdl_cache_dir', '/var/tmp'), ini_get('soap.wsdl_cache_dir'));
/// var_dump(ini_set('soap.wsdl_cache_dir', "a\0b"), ini_get('soap.wsdl_cache_dir'));
/// ```
/// ```expect
/// string(4) "/tmp"
/// string(8) "/var/tmp"
/// bool(false)
/// string(8) "/var/tmp"
/// ```
#[allow(clippy::unnecessary_wraps)] // the ABI's `on_modify`
fn on_update_cache_dir(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    if !matches!(req.ini_stage(), IniStage::Runtime) {
        return Ok(true);
    }
    let value = value.unwrap_or_default();
    if value.contains(&0) {
        return Ok(false);
    }
    let dir = match value.iter().position(|c| *c == b';') {
        Some(first) => {
            let rest = &value[first + 1..];
            match rest.iter().position(|c| *c == b';') {
                Some(second) => &rest[second + 1..],
                None => rest,
            }
        }
        None => value,
    };
    Ok(dir.is_empty() || req.check_open_basedir(dir, true).is_ok())
}

/// `PHP_MINFO_FUNCTION(soap)`: the two rows, then the ini entries.
///
/// ```php
/// (new ReflectionExtension('soap'))->info();
/// ```
/// ```expect
///
/// soap
///
/// Soap Client => enabled
/// Soap Server => enabled
///
/// Directive => Local Value => Master Value
/// soap.wsdl_cache_enabled => On => On
/// soap.wsdl_cache_dir => /tmp => /tmp
/// soap.wsdl_cache_ttl => 86400 => 86400
/// soap.wsdl_cache => 1 => 1
/// soap.wsdl_cache_limit => 5 => 5
/// ```
fn soap_info(req: &mut Request) {
    req.info_table(&[&["Soap Client", "enabled"], &["Soap Server", "enabled"]]);
    let Some(me) = req.extension_named(b"soap") else {
        return;
    };
    let rows: Vec<[String; 3]> = req
        .extension_ini(me)
        .into_iter()
        .map(|held| {
            let name = held.decl.name.0.to_vec();
            let shown = |value: Option<&[u8]>| {
                let value = value.unwrap_or_default();
                if name.as_slice() == ini::CACHE_ENABLED {
                    // `STD_PHP_INI_BOOLEAN`'s displayer.
                    String::from(if Request::ini_parse_bool(value) {
                        "On"
                    } else {
                        "Off"
                    })
                } else {
                    String::from_utf8_lossy(value).into_owned()
                }
            };
            [
                String::from_utf8_lossy(&name).into_owned(),
                shown(held.value.as_deref()),
                shown(held.master.as_deref()),
            ]
        })
        .collect();
    let rows: Vec<[&str; 3]> = rows
        .iter()
        .map(|[n, l, m]| [n.as_str(), l.as_str(), m.as_str()])
        .collect();
    req.info_ini_table(&rows);
}

/// `use_soap_error_handler(bool $enable = true): bool`: the switch set,
/// the old one answered.
///
/// ```php
/// var_dump(use_soap_error_handler(), use_soap_error_handler(false), use_soap_error_handler());
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn use_soap_error_handler(req: &mut Request, #[php(default = true)] enable: bool) -> bool {
    std::mem::replace(&mut req.state::<State>().use_soap_error_handler, enable)
}

/// `is_soap_fault(mixed $object): bool`: an instance of `SoapFault`.
///
/// ```php
/// class F extends SoapFault {}
/// var_dump(is_soap_fault(new SoapFault('Server', 'x')), is_soap_fault(new F('Client', 'y')), is_soap_fault(new Exception), is_soap_fault('SoapFault'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn is_soap_fault(req: &mut Request, object: Value) -> bool {
    match object.as_ref() {
        ValueRef::Object(o) => classes::is_fault(req, o),
        _ => false,
    }
}
