//! The `uri` extension: `php-src/ext/uri` as one crate under `ext/`
//! (ADR-0004), 8.5's `Uri\Rfc3986\Uri` over uriparser and
//! `Uri\WhatWg\Url` over lexbor. Two dependencies: the facade, and
//! `ext/lexbor`, whose URL parser php's `uri_parser_whatwg.c` includes
//! (design §4.10); one entry, `extension()`; one request state, the
//! WHATWG parser php's `RINIT` lays (ADR-0005). The phpt subset is
//! `php-src/ext/uri/tests`, through `xtask phpt --subset`.
//!
//! This file is `php_uri.c` and `php_uri_common.c`: the classes the two
//! standards share, and the class machinery written once over
//! [`UriParser`], php's `php_uri_parser`, which [`rfc3986`] and
//! [`whatwg`] each implement. uriparser is vendored and bound in [`ffi`],
//! the crate's one `unsafe` home.
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

mod ffi;
mod rfc3986;
mod whatwg;

use std::marker::PhantomData;

use php_ext::abi::{
    CloneObj, Dependency, DependencyKind, Extension, ExtensionBuilder, ObjectHandlers, Ref, Static,
    Untyped,
};
use php_ext::attr::php_class;
use php_ext::call::{clone_with, lookup_class, new_object};
use php_ext::raising::{Thrown, fatal, throw_chained, throw_error, throw_exception_ex};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, Owner, PhpArray, PhpObject, PhpString, Value};

/// `php_uri_component_read_mode`: how a component is read, as it was
/// written or normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadAs {
    Raw,
    NormalizedAscii,
    NormalizedUnicode,
}

/// `php_uri_recomposition_mode`: how a URI is written back as a string;
/// php's fourth, normalized Unicode, no method asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recomposition {
    RawAscii,
    RawUnicode,
    NormalizedAscii,
}

impl Recomposition {
    /// The read a recomposition of this kind makes of the URI.
    fn read_as(self) -> ReadAs {
        match self {
            Recomposition::RawAscii | Recomposition::RawUnicode => ReadAs::Raw,
            Recomposition::NormalizedAscii => ReadAs::NormalizedAscii,
        }
    }
}

/// What a parse left: the URI, `None` where the text is not one, and the
/// errors array where the caller asked for one (`zval *errors`).
pub(crate) struct Parsed<P> {
    pub(crate) uri: Option<P>,
    pub(crate) errors: Option<Owner<PhpArray>>,
}

/// A component the parser could not read: a `read` handler's
/// `FAILURE`, which `php_uri_property_read_helper` throws as a
/// `Uri\UriError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unreadable;

/// What one component read answers.
pub(crate) type Read<T> = Result<T, Unreadable>;

/// `php_uri_parser`: one URI standard, the parse, the copy, the
/// recomposition and the read and write of each component, which the
/// class machinery below calls and never looks past.
pub(crate) trait UriParser: Sized + Send + 'static {
    /// `php_uri_parser.name`: the class the standard is served as.
    const CLASS: &'static str;

    /// `parse`: `text` resolved against `base` where there is one. With
    /// `silent` unset a failure is the throw the `Err` carries; with it
    /// set a failure is `uri: None`. The errors array is made when
    /// `want_errors`.
    fn parse(
        req: &mut Request,
        text: &[u8],
        base: Option<&Self>,
        want_errors: bool,
        silent: bool,
    ) -> Result<Parsed<Self>, Thrown>;

    /// `clone`: a deep copy, `None` where the library could not allocate;
    /// the `Err` is the memory limit's fatal.
    fn duplicate(&self, req: &mut Request) -> Result<Option<Self>, Thrown>;

    /// What a read in `mode` needs laid down before it, allocated on the
    /// request's heap: `get_normalized_uri`'s copy, which RFC 3986 makes
    /// on the first read that is not raw. Nothing by default.
    fn prepare(&mut self, req: &mut Request, mode: ReadAs) -> Result<(), Thrown> {
        let _ = (req, mode);
        Ok(())
    }

    /// The string a username or a password read is: `ZVAL_STRINGL`'s
    /// ([`PhpString::new`]) by default.
    fn credential(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
        PhpString::new(req, bytes)
    }

    /// `free`: what the parser holds on the request's heap given back,
    /// as `php_uri_object_handler_free` frees it.
    fn free(self, req: &mut Request) {
        let _ = req;
    }

    /// `to_string`, `None` where the URI cannot be recomposed; the `Err`
    /// is the memory limit's fatal, met where the recomposition
    /// allocates on the request's heap.
    fn recompose(
        &mut self,
        req: &mut Request,
        mode: Recomposition,
        exclude_fragment: bool,
    ) -> Result<Option<Vec<u8>>, Thrown>;

    fn scheme(&mut self, mode: ReadAs) -> Read<Option<Vec<u8>>>;
    fn username(&mut self, mode: ReadAs) -> Read<Option<Vec<u8>>>;
    fn password(&mut self, mode: ReadAs) -> Read<Option<Vec<u8>>>;
    /// The host, which a read in Unicode converts on the request's heap;
    /// the `Err` is the memory limit's fatal.
    fn host(&mut self, req: &mut Request, mode: ReadAs) -> Result<Read<Option<Vec<u8>>>, Thrown>;
    fn port(&mut self, mode: ReadAs) -> Read<Option<i64>>;
    fn path(&mut self, mode: ReadAs) -> Read<Vec<u8>>;
    fn query(&mut self, mode: ReadAs) -> Read<Option<Vec<u8>>>;
    fn fragment(&mut self, mode: ReadAs) -> Read<Option<Vec<u8>>>;

    fn set_scheme(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown>;
    fn set_host(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown>;
    fn set_port(&mut self, req: &mut Request, value: Option<i64>) -> Result<(), Thrown>;
    fn set_path(&mut self, req: &mut Request, value: &[u8]) -> Result<(), Thrown>;
    fn set_query(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown>;
    fn set_fragment(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown>;
}

/// A parser `php_uri_get_parser` finds by name, what `php_uri.h` exports
/// to ext/filter's `FILTER_VALIDATE_URL` (design §4.10). php's
/// `uri_parsers` table holds a third, `parse_url`, which
/// `uri_parser_php_parse_url.c` makes of ext/standard's
/// `php_url_parse_ex2`; here it stays with
/// ext/standard, whose http and ftp wrappers need this crate
/// (`ZEND_MOD_REQUIRED("uri")`), so a caller that names no class, or
/// names `parse_url`, reads `parse_url` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parser {
    /// `php_uri_parser_rfc3986`, `Uri\Rfc3986\Uri`.
    Rfc3986,
    /// `php_uri_parser_whatwg`, `Uri\WhatWg\Url`.
    WhatWg,
}

/// `php_uri`: the components `php_uri_parse_to_struct` read, each
/// string that was one and the number that was an integer.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UriStruct {
    pub scheme: Option<Vec<u8>>,
    pub user: Option<Vec<u8>>,
    pub password: Option<Vec<u8>>,
    pub host: Option<Vec<u8>>,
    pub port: Option<i64>,
    pub path: Option<Vec<u8>>,
    pub query: Option<Vec<u8>>,
    pub fragment: Option<Vec<u8>>,
}

impl Parser {
    /// `php_uri_get_parser`: the parser registered under the class name
    /// `name`.
    #[must_use]
    pub fn named(name: &[u8]) -> Option<Parser> {
        [Parser::Rfc3986, Parser::WhatWg]
            .into_iter()
            .find(|parser| parser.name().as_bytes() == name)
    }

    /// `php_uri_parser.name`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Parser::Rfc3986 => rfc3986::Rfc3986::CLASS,
            Parser::WhatWg => whatwg::WhatWg::CLASS,
        }
    }

    /// `php_uri_parse_to_struct`: `text` parsed and each component read
    /// as `form` asks; `None` where the text is no URI or a component
    /// cannot be read, with the parser's throw unless `silent`.
    ///
    /// ```php
    /// foreach ([Uri\Rfc3986\Uri::class, Uri\WhatWg\Url::class] as $p) {
    ///     foreach (["http:///example.com", "http://exa%23mple.org", "http://u%zz@h.com"] as $u) echo $p, " ", var_export(filter_var($u, FILTER_VALIDATE_URL, ["options" => ["uri_parser_class" => $p]]), true), "\n";
    /// }
    /// ```
    /// ```expect
    /// Uri\Rfc3986\Uri 'http:///example.com'
    /// Uri\Rfc3986\Uri 'http://exa%23mple.org'
    /// Uri\Rfc3986\Uri false
    /// Uri\WhatWg\Url 'http:///example.com'
    /// Uri\WhatWg\Url false
    /// Uri\WhatWg\Url 'http://u%zz@h.com'
    /// ```
    pub fn parse_to_struct(
        self,
        req: &mut Request,
        text: &[u8],
        form: ReadAs,
        silent: bool,
    ) -> Result<Option<UriStruct>, Thrown> {
        match self {
            Parser::Rfc3986 => to_struct::<rfc3986::Rfc3986>(req, text, form, silent),
            Parser::WhatWg => to_struct::<whatwg::WhatWg>(req, text, form, silent),
        }
    }
}

fn to_struct<P: UriParser>(
    req: &mut Request,
    text: &[u8],
    form: ReadAs,
    silent: bool,
) -> Result<Option<UriStruct>, Thrown> {
    let Some(mut uri) = P::parse(req, text, None, false, silent)?.uri else {
        return Ok(None);
    };
    let read = uri
        .prepare(req, form)
        .and_then(|()| components(req, &mut uri, form));
    uri.free(req);
    Ok(read?.ok())
}

/// Each component in `php_uri_parse_to_struct`'s order, the first a
/// parser cannot read ending the read.
fn components<P: UriParser>(
    req: &mut Request,
    uri: &mut P,
    form: ReadAs,
) -> Result<Read<UriStruct>, Thrown> {
    let host = uri.host(req, form)?;
    Ok((|| {
        Ok(UriStruct {
            scheme: uri.scheme(form)?,
            user: uri.username(form)?,
            password: uri.password(form)?,
            host: host?,
            port: uri.port(form)?,
            path: Some(uri.path(form)?),
            query: uri.query(form)?,
            fragment: uri.fragment(form)?,
        })
    })())
}

/// `php_uri_object`'s `uri`: `None` until a constructor, `parse()`,
/// `resolve()` or `__unserialize()` has set it.
pub(crate) struct Slot<P>(Option<P>);

impl<P> Default for Slot<P> {
    fn default() -> Self {
        Slot(None)
    }
}

/// The class's handler table: `php_uri_object_handler_clone`, the URI
/// copied into the clone, and `php_uri_object_handler_free`.
pub(crate) struct Handlers<P>(PhantomData<fn() -> P>);

impl<P: UriParser> ObjectHandlers for Handlers<P> {
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_uri::<P>)
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let uri = object
            .native_mut::<Slot<P>>(req)
            .and_then(|slot| slot.0.take());
        if let Some(uri) = uri {
            uri.free(req);
        }
    }
}

fn clone_uri<P: UriParser>(
    req: &mut Request,
    object: PhpObject,
) -> Result<Owner<PhpObject>, Thrown> {
    let copy = with_uri::<P, _>(req, object, |req, uri| uri.duplicate(req))?;
    let Some(copy) = copy else {
        return Err(fatal(req, "InternalError: a URI could not be copied"));
    };
    clone_with(req, &object, Slot(Some(copy)))
}

/// `f` over the object's URI, taken out of its slot while `f` runs and
/// put back after; a URI never set is php's failed `ZEND_ASSERT`, the one
/// `InternalError`.
fn with_uri<P: UriParser, R>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut Request, &mut P) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let Some(mut uri) = object
        .native_mut::<Slot<P>>(req)
        .and_then(|slot| slot.0.take())
    else {
        return Err(fatal(
            req,
            &format!(
                "InternalError: a {} served before its URI was set",
                P::CLASS
            ),
        ));
    };
    let result = f(req, &mut uri);
    if let Some(slot) = object.native_mut::<Slot<P>>(req) {
        slot.0 = Some(uri);
    }
    result
}

/// Whether the object's URI is set.
fn is_set<P: UriParser>(req: &Request, object: PhpObject) -> bool {
    object
        .native::<Slot<P>>(req)
        .is_some_and(|slot| slot.0.is_some())
}

/// Where `php_uri_instantiate_uri` puts the URI it parses.
#[derive(Clone, Copy)]
enum Target {
    /// The constructor's receiver, `should_update_this_object`.
    This(PhpObject),
    /// A fresh object of the class, returned.
    New(ClassId),
}

/// `php_uri_instantiate_uri`: `text` parsed, against the base object's
/// URI where there is one, into the receiver or a fresh object; a
/// failure is the parser's throw with `should_throw`, and `None` without.
/// The errors array goes to `errors` where the call gave the reference.
fn instantiate<P: UriParser>(
    req: &mut Request,
    target: Target,
    text: &[u8],
    base: Option<PhpObject>,
    should_throw: bool,
    errors: Option<&Ref<Untyped<Value>>>,
) -> Result<Option<Owner<PhpObject>>, Thrown> {
    let (object, fresh) = match target {
        Target::This(object) => {
            if is_set::<P>(req, object) {
                return Err(readonly(req, P::CLASS));
            }
            (object, None)
        }
        Target::New(class) => {
            let fresh = new_object(req, class)?;
            (*fresh, Some(fresh))
        }
    };
    let want_errors = errors.is_some_and(Ref::is_given);
    let parsed = match base {
        Some(base) => with_uri::<P, _>(req, base, |req, base| {
            P::parse(req, text, Some(base), want_errors, !should_throw)
        }),
        None => P::parse(req, text, None, want_errors, !should_throw),
    };
    let parsed = parsed.and_then(|parsed| {
        match (parsed.errors, errors) {
            (Some(array), Some(errors)) => {
                errors.set(req, Untyped(array.into_value()))?;
            }
            (Some(array), None) => array.give_up(req),
            (None, _) => {}
        }
        Ok(parsed.uri)
    });
    match parsed {
        Ok(Some(uri)) => {
            if let Some(slot) = object.native_mut::<Slot<P>>(req) {
                slot.0 = Some(uri);
            }
            Ok(Some(match fresh {
                Some(fresh) => fresh,
                None => Owner::take(req, object),
            }))
        }
        failed => {
            if let Some(fresh) = fresh {
                fresh.give_up(req);
            }
            failed.map(|_| None)
        }
    }
}

/// "Cannot modify readonly object of class ..."
fn readonly(req: &mut Request, class: &str) -> Thrown {
    throw_error(
        req,
        "Error",
        &format!("Cannot modify readonly object of class {class}"),
    )
}

/// The static `parse()` of either class, `create_*_uri` with
/// `is_constructor` unset: a fresh object of the called class, or `null`.
fn parse_static<P: UriParser>(
    req: &mut Request,
    text: PhpString,
    base: Option<PhpObject>,
    errors: Option<&Ref<Untyped<Value>>>,
) -> Result<Option<Owner<Static>>, Thrown> {
    let class = match req.frames().called_scope() {
        Some(class) => class,
        None => match lookup_class(req, P::CLASS.as_bytes())? {
            Some(class) => class,
            None => return Err(fatal(req, "InternalError: a URI class is not linked")),
        },
    };
    let text = text.as_bytes().to_vec();
    let made = instantiate::<P>(req, Target::New(class), &text, base, false, errors)?;
    Ok(made.map(Owner::into_static))
}

/// The constructor of either class, `create_*_uri` with
/// `is_constructor` set: the receiver's URI, once.
fn construct<P: UriParser>(
    req: &mut Request,
    this: PhpObject,
    text: PhpString,
    base: Option<PhpObject>,
    errors: Option<&Ref<Untyped<Value>>>,
) -> Result<(), Thrown> {
    let text = text.as_bytes().to_vec();
    let made = instantiate::<P>(req, Target::This(this), &text, base, true, errors)?;
    if let Some(made) = made {
        made.give_up(req);
    }
    Ok(())
}

/// `resolve()`: `text` against the receiver, into a fresh object of its
/// class.
fn resolve<P: UriParser>(
    req: &mut Request,
    this: PhpObject,
    text: PhpString,
    errors: Option<&Ref<Untyped<Value>>>,
) -> Result<Owner<Static>, Thrown> {
    let text = text.as_bytes().to_vec();
    let made = instantiate::<P>(
        req,
        Target::New(this.class()),
        &text,
        Some(this),
        true,
        errors,
    )?;
    match made {
        Some(object) => Ok(object.into_static()),
        None => Err(fatal(
            req,
            "InternalError: a resolution that throws returned nothing",
        )),
    }
}

/// Whatever `f` makes of the receiver's URI read in `mode`, what the
/// read needs laid down first ([`UriParser::prepare`]).
fn read<P: UriParser, T>(
    req: &mut Request,
    this: PhpObject,
    mode: ReadAs,
    f: impl FnOnce(&mut Request, &mut P) -> Result<T, Thrown>,
) -> Result<T, Thrown> {
    with_uri::<P, _>(req, this, |req, uri| {
        uri.prepare(req, mode)?;
        f(req, uri)
    })
}

/// One component read, `php_uri_property_read_helper`: a component the
/// parser cannot read is the `UriError` naming it.
fn component<P: UriParser, T>(
    req: &mut Request,
    this: PhpObject,
    name: &str,
    mode: ReadAs,
    f: impl FnOnce(&mut Request, &mut P) -> Result<Read<T>, Thrown>,
) -> Result<T, Thrown> {
    match read::<P, _>(req, this, mode, f)? {
        Ok(value) => Ok(value),
        Err(Unreadable) => Err(throw_exception_ex(
            req,
            "Uri\\UriError",
            &format!("The {name} component cannot be retrieved"),
        )),
    }
}

/// A string component read.
fn component_string<P: UriParser>(
    req: &mut Request,
    this: PhpObject,
    name: &str,
    mode: ReadAs,
    f: impl FnOnce(&mut Request, &mut P) -> Result<Read<Option<Vec<u8>>>, Thrown>,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let bytes = component::<P, _>(req, this, name, mode, f)?;
    bytes.map(|bytes| PhpString::new(req, &bytes)).transpose()
}

/// The username or the password read, made as the parser makes it
/// ([`UriParser::credential`]).
fn component_credential<P: UriParser>(
    req: &mut Request,
    this: PhpObject,
    name: &str,
    mode: ReadAs,
    f: impl FnOnce(&mut Request, &mut P) -> Result<Read<Option<Vec<u8>>>, Thrown>,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let bytes = component::<P, _>(req, this, name, mode, f)?;
    bytes.map(|bytes| P::credential(req, &bytes)).transpose()
}

/// `php_uri_property_write_helper`: the receiver cloned through its
/// handler, `f` applied to the clone's URI, and the clone returned; a refused
/// write is the throw `f` made, the clone released.
fn write<P: UriParser>(
    req: &mut Request,
    this: PhpObject,
    f: impl FnOnce(&mut Request, &mut P) -> Result<(), Thrown>,
) -> Result<Owner<Static>, Thrown> {
    let copy = clone_uri::<P>(req, this)?;
    match with_uri::<P, _>(req, *copy, f) {
        Ok(()) => Ok(copy.into_static()),
        Err(thrown) => {
            copy.give_up(req);
            Err(thrown)
        }
    }
}

/// A write of a string or `null`.
fn write_bytes<P: UriParser>(
    req: &mut Request,
    this: PhpObject,
    value: Option<PhpString>,
    f: impl FnOnce(&mut P, &mut Request, Option<&[u8]>) -> Result<(), Thrown>,
) -> Result<Owner<Static>, Thrown> {
    let value = value.map(|value| value.as_bytes().to_vec());
    write::<P>(req, this, |req, uri| f(uri, req, value.as_deref()))
}

/// `throw_cannot_recompose_uri_to_string`.
fn cannot_recompose(req: &mut Request, class: &str) -> Thrown {
    throw_exception_ex(
        req,
        "Uri\\UriError",
        &format!("Cannot recompose {class} to a string"),
    )
}

/// `to_string` of the receiver in `mode`, php's `UriError` where it
/// cannot be.
fn recompose<P: UriParser>(
    req: &mut Request,
    this: PhpObject,
    mode: Recomposition,
) -> Result<Owner<PhpString>, Thrown> {
    let bytes = read::<P, _>(req, this, mode.read_as(), |req, uri| {
        uri.recompose(req, mode, false)
    })?;
    match bytes {
        Some(bytes) => PhpString::new(req, &bytes),
        None => Err(cannot_recompose(req, P::CLASS)),
    }
}

/// `uri_equals`: the two recomposed as normalized ASCII, the fragment
/// left out unless `IncludeFragment` is asked for.
fn equals<P: UriParser>(
    req: &mut Request,
    this: PhpObject,
    that: PhpObject,
    mode: UriComparisonMode,
) -> Result<bool, Thrown> {
    let exclude_fragment = mode == UriComparisonMode::ExcludeFragment;
    let side = |req: &mut Request, object| {
        let bytes = read::<P, _>(req, object, ReadAs::NormalizedAscii, |req, uri| {
            uri.recompose(req, Recomposition::NormalizedAscii, exclude_fragment)
        })?;
        bytes.ok_or_else(|| cannot_recompose(req, P::CLASS))
    };
    let mine = side(req, this)?;
    let theirs = side(req, that)?;
    Ok(mine == theirs)
}

/// `PHP_URI_SERIALIZE_URI_FIELD_NAME`.
const URI_FIELD: &[u8] = b"uri";

/// `__serialize()`: `[["uri" => raw ASCII string], []]`.
fn serialize<P: UriParser>(req: &mut Request, this: PhpObject) -> Result<Owner<PhpArray>, Thrown> {
    let uri = recompose::<P>(req, this, Recomposition::RawAscii)?;
    let mut state = PhpArray::new(req)?;
    state.set_str(req, URI_FIELD, uri.into_value())?;
    let mut out = PhpArray::new(req)?;
    out.append(req, state.into_value())?;
    out.append(req, Owner::take(req, Value::array(PhpArray::empty())))?;
    Ok(out)
}

/// `uri_unserialize`: the state `__serialize()` wrote, parsed silently
/// into an object not yet set; any other shape is php's `Exception`.
fn unserialize<P: UriParser>(
    req: &mut Request,
    this: PhpObject,
    data: PhpArray,
) -> Result<(), Thrown> {
    let invalid = format!("Invalid serialization data for {} object", P::CLASS);
    if is_set::<P>(req, this) {
        let first = readonly(req, P::CLASS);
        // `zend_throw_exception_ex`, whose `zend_vstrpprintf` string is
        // the message itself, so nothing stands across the throw.
        return Err(throw_chained(req, first, "Exception", &invalid));
    }
    let Some(text) = serialized_uri(data) else {
        return Err(throw_exception_ex(req, "Exception", &invalid));
    };
    let parsed = P::parse(req, &text, None, false, true)?;
    let Some(uri) = parsed.uri else {
        return Err(throw_exception_ex(req, "Exception", &invalid));
    };
    if let Some(slot) = this.native_mut::<Slot<P>>(req) {
        slot.0 = Some(uri);
    }
    let no_properties = data
        .get(&Key::Int(1))
        .and_then(|value| value.as_array().map(|v| v.is_empty()))
        .unwrap_or(false);
    if no_properties {
        Ok(())
    } else {
        Err(throw_exception_ex(req, "Exception", &invalid))
    }
}

/// The `uri` string of `[["uri" => ...], [...]]`, checked as
/// `uri_unserialize` checks it before the parse: two elements, the first
/// an array of the one string.
fn serialized_uri(data: PhpArray) -> Option<Vec<u8>> {
    if data.len() != 2 {
        return None;
    }
    let state = data.get(&Key::Int(0))?;
    let state = state.as_array()?;
    if state.len() != 1 {
        return None;
    }
    let uri = state.find(URI_FIELD)?;
    uri.as_str_bytes().map(<[u8]>::to_vec)
}

/// `uri_get_debug_properties`: the declared properties, none, and each
/// component read raw, in php's order; the properties alone where the URI
/// was never set.
fn debug_info<P: UriParser>(req: &mut Request, this: PhpObject) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for (key, value) in this.props(req)? {
        out.set(req, Owner::take(req, key), Owner::take(req, value))?;
    }
    if !is_set::<P>(req, this) {
        return Ok(out);
    }
    let raw = ReadAs::Raw;
    let read = read::<P, _>(req, this, raw, |req, u| {
        Ok((
            u.scheme(raw),
            u.username(raw),
            u.password(raw),
            u.host(req, raw)?,
            u.port(raw),
            u.path(raw),
            u.query(raw),
            u.fragment(raw),
        ))
    });
    let (scheme, username, password, host, port, path, query, fragment) = match read {
        Ok(read) => read,
        Err(thrown) => {
            out.give_up(req);
            return Err(thrown);
        }
    };
    // A component the parser cannot read is left out, as php's
    // `== SUCCESS` checks leave it.
    if let Ok(value) = scheme {
        put_string(req, &mut out, b"scheme", value)?;
    }
    for (name, value) in [(b"username".as_slice(), username), (b"password", password)] {
        if let Ok(value) = value {
            let value = match value {
                Some(bytes) => P::credential(req, &bytes)?.into_value(),
                None => Owner::null(),
            };
            put(req, &mut out, name, value)?;
        }
    }
    if let Ok(value) = host {
        put_string(req, &mut out, b"host", value)?;
    }
    if let Ok(port) = port {
        put(
            req,
            &mut out,
            b"port",
            Owner::take(req, port.map_or_else(Value::null, Value::int)),
        )?;
    }
    if let Ok(path) = path {
        put_string(req, &mut out, b"path", Some(path))?;
    }
    for (name, value) in [(b"query".as_slice(), query), (b"fragment", fragment)] {
        if let Ok(value) = value {
            put_string(req, &mut out, name, value)?;
        }
    }
    Ok(out)
}

fn put_string(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    name: &[u8],
    value: Option<Vec<u8>>,
) -> Result<(), Thrown> {
    let value = match value {
        Some(bytes) => PhpString::new(req, &bytes)?.into_value(),
        None => Owner::null(),
    };
    put(req, array, name, value)
}

fn put(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    name: &[u8],
    value: Owner<Value>,
) -> Result<(), Thrown> {
    array.set_str(req, name, value)
}

/// Core's `Exception`, `Uri\UriException`'s parent.
#[php_class(extern)]
pub(crate) struct Exception;

/// Core's `Error`, `Uri\UriError`'s parent.
#[php_class(extern)]
pub(crate) struct Error;

/// `class Uri\UriException extends \Exception`, `@strict-properties`.
///
/// ```php
/// var_dump(get_parent_class('Uri\UriException'), get_parent_class('Uri\InvalidUriException'), get_parent_class('Uri\UriError'));
/// ```
/// ```expect
/// string(9) "Exception"
/// string(16) "Uri\UriException"
/// string(5) "Error"
/// ```
#[php_class(namespace(Uri), extends(Exception), dynamic_properties = forbidden)]
pub(crate) struct UriException;

#[php_class]
impl UriException {}

/// `class Uri\UriError extends \Error`, `@strict-properties`.
#[php_class(namespace(Uri), extends(Error), dynamic_properties = forbidden)]
pub(crate) struct UriError;

#[php_class]
impl UriError {}

/// `class Uri\InvalidUriException extends Uri\UriException`,
/// `@strict-properties`.
#[php_class(
    namespace(Uri),
    extends(UriException),
    dynamic_properties = forbidden
)]
pub(crate) struct InvalidUriException;

#[php_class]
impl InvalidUriException {}

/// `enum Uri\UriComparisonMode`: whether `equals()` compares the
/// fragment.
///
/// ```php
/// var_dump(Uri\UriComparisonMode::cases());
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   enum(Uri\UriComparisonMode::IncludeFragment)
///   [1]=>
///   enum(Uri\UriComparisonMode::ExcludeFragment)
/// }
/// ```
#[php_class(namespace(Uri))]
pub(crate) enum UriComparisonMode {
    IncludeFragment,
    ExcludeFragment,
}

#[php_class]
impl UriComparisonMode {}

/// The extension value (design §7.1): `uri_module_entry`, which requires
/// `lexbor`, with its classes in `PHP_MINIT_FUNCTION(uri)`'s order, and
/// `PHP_RINIT(uri)` and its `post_deactivate`, which lay and destroy the
/// request's WHATWG parser.
///
/// ```php
/// var_dump(extension_loaded('uri'), (new ReflectionExtension('uri'))->getDependencies());
/// ```
/// ```expect
/// bool(true)
/// array(1) {
///   ["lexbor"]=>
///   string(8) "Required"
/// }
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .dependencies(vec![Dependency {
            name: String::from("lexbor"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .class::<rfc3986::Uri>()
        .class::<whatwg::Url>()
        .class::<UriComparisonMode>()
        .class::<UriException>()
        .class::<UriError>()
        .class::<InvalidUriException>()
        .class::<whatwg::InvalidUrlException>()
        .class::<whatwg::UrlValidationError>()
        .class::<whatwg::UrlValidationErrorType>()
        .request_state::<whatwg::State>()
        .rinit(whatwg::rinit)
        .post_deactivate(whatwg::post_deactivate)
        .info(uri_info)
        .build("uri")
}

/// `PHP_MINFO_FUNCTION(uri)`: the bundled uriparser's `URI_VER_ANSI`.
///
/// ```php
/// (new ReflectionExtension('uri'))->info();
/// ```
/// ```expect
///
/// uri
///
/// URI support => active
/// uriparser bundled version => 1.0.2
/// ```
fn uri_info(req: &mut Request) {
    req.extensions_mut().info_table(&[
        &["URI support", "active"],
        &["uriparser bundled version", "1.0.2"],
    ]);
}
