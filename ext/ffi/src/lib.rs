//! The `ffi` extension: `php-src/ext/ffi` as one crate under `ext/`
//! (ADR-0004), `ffi.c` and its C-declaration parser ported over the
//! platform's libffi, as the oracle links it, bound by hand in [`ffi`]
//! (design §7.2, revision 75; #1573). This file is the extension value,
//! its ini entries and its state; [`decls`] is the `FFI` class,
//! [`cdata`] `FFI\CData`, [`ctype`] `FFI\CType`, [`call`] the call
//! into C and back, [`types`] the C type model and [`parse`] the
//! parser. FFI calls C and reads C memory; it loads no extension, which
//! is §7.1's ruling against `dl()` and stands. The phpt subset is
//! `php-src/ext/ffi/tests`, through `xtask phpt --subset`.
#![deny(unsafe_code)]
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
// The port is php's C, whose conversions between the widths of C's
// integers and doubles truncate, wrap and lose precision where php's
// casts do; those casts are the behaviour.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless
)]

mod call;
mod cdata;
mod ctype;
mod decls;
mod ffi;
mod parse;
mod preload;
mod types;

use std::collections::HashMap;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::php_class;
use php_ext::call::lookup_class;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{Thrown, fatal, throw, throw_error};
use php_ext::request::{Ini, Request};
use php_ext::values::{ClassId, Emalloc};

use crate::types::Types;

/// `ffi.enable`.
const ENABLE: &str = "ffi.enable";
/// `ffi.preload`.
pub(crate) const PRELOAD: &str = "ffi.preload";

/// Request-scoped (ADR-0005): the C types this request's declarations
/// made, the callbacks C may still call, the [`Reentry`] a callback
/// reaches the request through, and the memory `FFI` took from the
/// request's heap by its address ([`cdata::alloc`]). Dropped after `rshutdown`, which frees
/// every callback's code as php's `RSHUTDOWN` destroys
/// `FFI_G(callbacks)`.
#[derive(Default)]
struct State {
    types: Types,
    callbacks: Vec<call::Callback>,
    classes: Option<Classes>,
    imported: Vec<(Vec<u8>, parse::Scope)>,
    heap: HashMap<usize, Emalloc>,
    weak_types: Option<cdata::WeakTypes>,
}

/// The classes the extension declares, looked up once per request.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Classes {
    pub(crate) ffi: ClassId,
    pub(crate) cdata: ClassId,
    pub(crate) ctype: ClassId,
}

/// The extension's classes, or the fatal that says one is not linked.
pub(crate) fn classes(req: &mut Request) -> Result<Classes, Thrown> {
    if let Some(classes) = req.state::<State>().classes {
        return Ok(classes);
    }
    let mut found = [None; 3];
    for (slot, name) in found
        .iter_mut()
        .zip([b"FFI".as_slice(), b"FFI\\CData", b"FFI\\CType"])
    {
        *slot = lookup_class(req, name)?;
    }
    let [Some(ffi), Some(cdata), Some(ctype)] = found else {
        return Err(fatal(req, "InternalError: FFI's classes are not linked"));
    };
    let classes = Classes { ffi, cdata, ctype };
    req.state::<State>().classes = Some(classes);
    Ok(classes)
}

/// The request's type arena.
pub(crate) fn types(req: &mut Request) -> &mut Types {
    &mut req.state::<State>().types
}

/// `zend_throw_error(zend_ffi_exception_ce, ...)`.
pub(crate) fn ffi_error(req: &mut Request, message: &str) -> Thrown {
    throw_error(req, "FFI\\Exception", message)
}

/// `zend_ffi_parser_error`'s throw, `zend_throw_exception` of an
/// `FFI\ParserException`.
pub(crate) fn parser_error(req: &mut Request, message: &str) -> Thrown {
    throw(req, "FFI\\ParserException", message)
}

/// `zend_ffi_api_restriction`, what `ffi.enable` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Restriction {
    Disabled,
    Enabled,
    Preload,
}

/// `OnUpdateFFIEnable`: `preload` by any case, else the boolean.
fn restriction(req: &Request) -> Restriction {
    let value = req.ini().get(ENABLE.as_bytes()).unwrap_or(b"preload");
    if value.eq_ignore_ascii_case(b"preload") {
        Restriction::Preload
    } else if Ini::parse_bool(value) {
        Restriction::Enabled
    } else {
        Restriction::Disabled
    }
}

/// `ZEND_FFI_VALIDATE_API_RESTRICTION`: `ffi.enable=1` anywhere, and
/// under `preload` the CLI, whose scripts php lets use FFI after
/// startup, a function `opcache.preload`'s file declared, in any host
/// and any request, and the request that file is served in.
///
/// Under php-cgi, whose `ffi.enable` is `preload`, a function and a
/// method `ext/ffi/tests/preload.php` declared call FFI in a request
/// after the preload, and the request's own code is refused (#1614):
///
/// ```php
/// var_dump(preloaded_abs(-7), PreloadedFfi::abs(-2));
/// try { FFI::cdef("int abs(int j);"); } catch (FFI\Exception $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```request
/// --GET--
/// q=1
/// ```
/// ```expect regime=ffi-preload exit=0
/// X-Powered-By: PHP/8.5.9
/// Content-type: text/html; charset=UTF-8
///
/// int(7)
/// int(2)
/// FFI\Exception: FFI API is restricted by "ffi.enable" configuration directive
/// ```
pub(crate) fn validate_api_restriction(req: &mut Request) -> Result<(), Thrown> {
    match restriction(req) {
        Restriction::Enabled => Ok(()),
        Restriction::Preload
            if req.sapi_name() == "cli" || req.caller_preloaded() || req.preloading() =>
        {
            Ok(())
        }
        _ => Err(ffi_error(
            req,
            "FFI API is restricted by \"ffi.enable\" configuration directive",
        )),
    }
}

/// `class FFI\Exception extends \Error`.
///
/// ```php
/// var_dump(get_parent_class('FFI\Exception'), get_parent_class('FFI\ParserException'), (new ReflectionClass('FFI\ParserException'))->isFinal());
/// ```
/// ```expect
/// string(5) "Error"
/// string(13) "FFI\Exception"
/// bool(true)
/// ```
#[php_class(namespace(FFI), extends(Error))]
pub(crate) struct Exception;

#[php_class]
impl Exception {}

/// `final class FFI\ParserException extends FFI\Exception`.
#[php_class(namespace(FFI), extends(Exception), final)]
pub(crate) struct ParserException;

#[php_class]
impl ParserException {}

/// Core's `Error`, `FFI\Exception`'s parent.
#[php_class(extern)]
pub(crate) struct Error;

/// The extension value (design §7.1): the two ini entries, the five
/// classes in `ffi.stub.php`'s order, the non-static `new`, `cast` and
/// `type` an `FFI` object's `get_method` answers, and the state.
///
/// ```php
/// var_dump(extension_loaded('ffi'), ini_get('ffi.enable'), ini_get('ffi.preload'));
/// ```
/// ```expect
/// bool(true)
/// string(7) "preload"
/// string(0) ""
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .ini(vec![
            IniDecl {
                name: Name(ENABLE.as_bytes().into()),
                default: Some(b"preload".as_slice().into()),
                user: false,
                per_dir: false,
                system: true,
            },
            IniDecl {
                name: Name(PRELOAD.as_bytes().into()),
                default: None,
                user: false,
                per_dir: false,
                system: true,
            },
        ])
        .class::<Exception>()
        .class::<ParserException>()
        .class::<decls::FFI>()
        .class::<cdata::CData>()
        .class::<ctype::CType>()
        .class::<decls::instance::FFI>()
        .request_state::<State>()
        .engine_state(preload::engine_state)
        .rshutdown(call::rshutdown)
        .interrupt(call::serve_deferred)
        .info(info)
        .build("FFI")
}

/// `PHP_MINFO_FUNCTION(ffi)`: the table, then `DISPLAY_INI_ENTRIES`,
/// `ffi.enable` shown as its displayer shows it.
///
/// ```php
/// $e = new ReflectionExtension('FFI'); ob_start(); $e->info(); echo ob_get_clean();
/// ```
/// ```expect
/// FFI
///
/// FFI support => enabled
///
/// Directive => Local Value => Master Value
/// ffi.enable => preload => preload
/// ffi.preload => no value => no value
/// ```
fn info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["FFI support", "enabled"]]);
    let entries = req.ini().entries();
    let shown = |name: &str, value: Option<&[u8]>| -> String {
        let value = value.unwrap_or_default();
        if name == ENABLE {
            String::from(if value.eq_ignore_ascii_case(b"preload") {
                "preload"
            } else if Ini::parse_bool(value) {
                "On"
            } else {
                "Off"
            })
        } else {
            String::from_utf8_lossy(value).into_owned()
        }
    };
    let rows: Vec<[String; 3]> = [ENABLE, PRELOAD]
        .iter()
        .map(|name| {
            let entry = entries
                .iter()
                .find(|e| e.decl.name.0.as_ref() == name.as_bytes());
            let local = entry.and_then(|e| e.value.as_deref());
            let master = entry.and_then(|e| e.master.as_deref());
            [(*name).to_string(), shown(name, local), shown(name, master)]
        })
        .collect();
    let cells: Vec<[&str; 3]> = rows
        .iter()
        .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
        .collect();
    req.ini_mut().info_ini_table(&cells);
}
