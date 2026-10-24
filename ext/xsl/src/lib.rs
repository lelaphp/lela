//! The `xsl` extension: `php-src/ext/xsl` as one crate under `ext/`
//! (ADR-0004), `php_xsl.c` and `xsltprocessor.c` over the platform's
//! libxslt and libexslt, which [`ffi`] binds (design §7.2, revision 72).
//! The documents are libxml2's, taken from ext/dom and ext/simplexml
//! through ext/libxml's surface as php's xsl includes `php_libxml.h`, and
//! the php functions a stylesheet calls are dom's table,
//! `xpath_callbacks.h`'s (design §4.10). The phpt subset is
//! `php-src/ext/xsl/tests`, through `xtask phpt --subset`.
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

use php_ext::abi::{
    Callable, Dependency, DependencyKind, Extension, ExtensionBuilder, GcTable, ObjectHandlers,
    OrFalse, Path, PropertyPlace, PropertyRead, Raw, Refusals, Std, This, True, Union,
};
use php_ext::attr::{php_class, php_const};
use php_ext::call::{autoload, new_object};
use php_ext::coerce::{truthy, with_string};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, argument_error, docref, pending, throw_error,
};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{ClassId, Key, Owner, PhpArray, PhpObject, PhpString, Value};
use php_ext_dom::{CallbacksAt, XPathCallbacks};
use php_ext_libxml::ffi::{ClassType, Kind, Node, NodeObject, Ns, Raised, Route};
use php_ext_libxml::{Errors, import_node};

/// `XSL_SECPREF_DEFAULT`: every write forbidden.
const SECPREF_DEFAULT: i64 = 32 | 8 | 4;

/// `register_php_xsl_symbols`: the constants in `php_xsl.stub.php`'s order,
/// the versions those of the headers this binding was written to, which
/// the platform's libraries are.
#[php_const]
#[allow(non_upper_case_globals)]
pub(crate) mod constants {
    pub(crate) const XSL_CLONE_AUTO: i64 = 0;
    pub(crate) const XSL_CLONE_NEVER: i64 = -1;
    pub(crate) const XSL_CLONE_ALWAYS: i64 = 1;
    pub(crate) const XSL_SECPREF_NONE: i64 = 0;
    pub(crate) const XSL_SECPREF_READ_FILE: i64 = 2;
    pub(crate) const XSL_SECPREF_WRITE_FILE: i64 = 4;
    pub(crate) const XSL_SECPREF_CREATE_DIRECTORY: i64 = 8;
    pub(crate) const XSL_SECPREF_READ_NETWORK: i64 = 16;
    pub(crate) const XSL_SECPREF_WRITE_NETWORK: i64 = 32;
    pub(crate) const XSL_SECPREF_DEFAULT: i64 = super::SECPREF_DEFAULT;
    pub(crate) const LIBXSLT_VERSION: i64 = 10135;
    pub(crate) const LIBXSLT_DOTTED_VERSION: &str = "1.1.35";
    pub(crate) const LIBEXSLT_VERSION: i64 = 820;
    pub(crate) const LIBEXSLT_DOTTED_VERSION: &str = "0.8.20";
}

/// The extension value (design §7.1); `PHP_MINIT_FUNCTION(xsl)`'s
/// libxslt registrations are its process start (`initialize`).
///
/// ```php
/// var_dump(extension_loaded('xsl'), XSL_SECPREF_DEFAULT, LIBXSLT_DOTTED_VERSION);
/// print_r((new ReflectionExtension('xsl'))->getDependencies());
/// (new ReflectionExtension('xsl'))->info();
/// ```
/// ```expect
/// bool(true)
/// int(44)
/// string(6) "1.1.35"
/// Array
/// (
///     [libxml] => Required
///     [dom] => Required
/// )
///
/// xsl
///
/// XSL => enabled
/// libxslt Version => 1.1.35
/// libxslt compiled against libxml Version => 2.9.13
/// EXSLT => enabled
/// libexslt Version => 0.8.20
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .dependencies(vec![dependency("libxml"), dependency("dom")])
        .process_state(|| Ok(initialize()))
        .constants(constants::declared())
        .class::<XSLTProcessor>()
        .info(xsl_info)
        .build("xsl")
}

/// The process scope's proof that libxslt and libexslt are started
/// (ADR-0010).
struct Initialized;

/// `PHP_MINIT_FUNCTION(xsl)`'s library half (`Z/ext/xsl/php_xsl.c:282-292`),
/// the process start: called
/// once for the process, the first time an engine carrying xsl is built,
/// after libxml's, as the bundle orders them. It cannot fail.
fn initialize() -> Initialized {
    ffi::init();
    Initialized
}

fn dependency(name: &str) -> Dependency {
    Dependency {
        name: String::from(name),
        kind: DependencyKind::Required,
        rel: None,
        version: None,
    }
}

/// `PHP_MINFO_FUNCTION(xsl)`: a version number as `major.minor.subminor`.
fn xsl_info(req: &mut Request) {
    let dotted = |v: i32| format!("{}.{}.{}", v / 10000, v % 10000 / 100, v % 100);
    let (xslt, xml) = ffi::versions();
    let (xslt, xml) = (dotted(xslt), dotted(xml));
    req.extensions_mut().info_table(&[
        &["XSL", "enabled"],
        &["libxslt Version", &xslt],
        &["libxslt compiled against libxml Version", &xml],
        &["EXSLT", "enabled"],
        &["libexslt Version", constants::LIBEXSLT_DOTTED_VERSION],
    ]);
}

fn class_name(req: &Request, class: ClassId) -> String {
    req.symbols()
        .class_decl(class)
        .map(|d| String::from_utf8_lossy(&d.name.0).into_owned())
        .unwrap_or_default()
}

// ---- the object

/// `xsl_object`: the stylesheet, its parameters by key in insertion
/// order, the security preferences, dom's table of php functions, the
/// profile's path and how many transformations are running on it.
#[derive(Default)]
pub(crate) struct Intern {
    sheet: Option<ffi::Sheet>,
    /// `transform_depth`: the transformations in progress, nested where
    /// a callback transforms again, during which `importStylesheet` is
    /// refused (php 8.5.12, GH-23730).
    transform_depth: u32,
    has_keys: bool,
    params: Vec<(Vec<u8>, Vec<u8>)>,
    security: i64,
    callbacks: XPathCallbacks,
    profiling: Option<Box<[u8]>>,
}

fn intern(req: &mut Request, object: PhpObject) -> Option<&mut Intern> {
    object.native_mut::<Intern>(req)
}

/// `xsl_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// `maxTemplateDepth` and `maxTemplateVars`, the properties php's handlers
/// hold to zero and above.
fn validated(name: &[u8]) -> bool {
    name == b"maxTemplateDepth" || name == b"maxTemplateVars"
}

fn read_int(req: &mut Request, object: PhpObject, name: &[u8]) -> Result<i64, Thrown> {
    let value = Std.read_property(req, object, name, PropertyRead::Silent)?;
    let n = value.as_int().unwrap_or(0);
    value.give_up(req);
    Ok(n)
}

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `xsl_objects_new`: the default preferences, and the two limits at
    /// libxslt's own.
    fn create_object(&self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        if let Some(i) = intern(req, object) {
            i.security = SECPREF_DEFAULT;
        }
        let (depth, vars) = ffi::limits();
        for (name, n) in [
            (&b"maxTemplateDepth"[..], depth),
            (b"maxTemplateVars", vars),
        ] {
            let name = PhpString::new(req, name)?;
            Std.write_property(req, object, name, Owner::int(n))?;
        }
        Ok(())
    }

    /// `xsl_objects_free_storage`: `zend_object_std_dtor`, then the
    /// callbacks given up and the sheet let go of.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        Std.free_obj(req, object);
        if let Some(i) = intern(req, object) {
            let (sheet, callbacks) = (i.sheet.take(), std::mem::take(&mut i.callbacks));
            if let Some(sheet) = sheet {
                sheet.close(req);
            }
            callbacks.release(req);
        }
    }

    /// `xsl_objects_read_property`: a write fetch of a validated property
    /// refused.
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        if validated(name) && matches!(read, PropertyRead::Write | PropertyRead::Unset) {
            let (class, name) = (
                class_name(req, object.class()),
                String::from_utf8_lossy(name),
            );
            let message = format!("Indirect modification of {class}::${name} is not allowed");
            return Err(throw_error(req, "Error", &message));
        }
        Std.read_property(req, object, name, read)
    }

    /// `xsl_objects_write_property_with_validation`: the value written with
    /// its coercions, and the old one put back where it is below zero.
    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        if !validated(name.as_bytes()) {
            return Std.write_property(req, object, name, value);
        }
        let shown = String::from_utf8_lossy(name.as_bytes()).into_owned();
        let read = read_int(req, object, name.as_bytes())
            .and_then(|old| Ok((old, PhpString::new(req, name.as_bytes())?)));
        let (old, again) = match read {
            Ok(read) => read,
            Err(thrown) => {
                name.give_up(req);
                value.give_up(req);
                return Err(thrown);
            }
        };
        let now = match Std.write_property(req, object, name, value) {
            Ok(()) => read_int(req, object, again.as_bytes()),
            Err(thrown) => Err(thrown),
        };
        let now = match now {
            Ok(now) => now,
            Err(thrown) => {
                again.give_up(req);
                return Err(thrown);
            }
        };
        if now < 0 {
            Std.write_property(req, object, again, Owner::int(old))?;
            let class = class_name(req, object.class());
            let message = format!("{class}::${shown} must be greater than or equal to 0");
            return Err(throw_error(req, "ValueError", &message));
        }
        again.give_up(req);
        Ok(())
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        if validated(name) {
            return Ok(PropertyPlace::Handlers);
        }
        Std.get_property_ptr_ptr(req, object, name)
    }

    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        if validated(name) {
            let (class, name) = (
                class_name(req, object.class()),
                String::from_utf8_lossy(name),
            );
            return Err(throw_error(
                req,
                "Error",
                &format!("Cannot unset {class}::${name}"),
            ));
        }
        Std.unset_property(req, object, name)
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(i) = object.native::<Intern>(req) {
            i.callbacks.visit(visit);
        }
        GcTable::Properties
    }
}

// ---- the stylesheet

/// `xsl_add_ns_defs` over the copy of a modern document, read off the
/// original as the copy mirrors it: each `xmlns` attribute made a
/// declaration of its element, which libxslt reads the stylesheet's
/// prefixes from, and the attribute kept in the `xmlns` namespace that
/// `xmlCopyDoc` could not find in the tree, as php's clone keeps it,
/// the declaration the copy's to free (`oldNs`).
fn add_ns_defs(source: Node, copy: Node, doc: Node) {
    let (mut s, mut c) = (source.children(), copy.children());
    while let (Some(sn), Some(cn)) = (s, c) {
        if sn.kind() == Kind::Element {
            let (mut sa, mut ca) = (sn.properties(), cn.properties());
            while let (Some(a), Some(b)) = (sa, ca) {
                let ns = a
                    .ns()
                    .filter(|ns| ns.href().as_deref() == Some(b"http://www.w3.org/2000/xmlns/"));
                if let Some(ns) = ns {
                    let prefix = ns.prefix().map(|_| a.name());
                    cn.new_ns(&a.attr_value(), prefix.as_deref());
                    if let Some(kept) = Ns::owned(ns.href().as_deref(), ns.prefix().as_deref()) {
                        kept.move_to_old(Some(doc));
                        b.set_ns(Some(kept));
                    }
                }
                (sa, ca) = (a.next(), b.next());
            }
        }
        add_ns_defs(sn, cn, doc);
        (s, c) = (sn.next(), cn.next());
    }
}

/// Whether the stylesheet declares an `xsl:key` at its top level, which
/// makes every transformation copy its document first.
fn declares_keys(doc: Node) -> bool {
    let mut at = doc.root().and_then(Node::children);
    while let Some(n) = at {
        let in_xslt =
            n.ns().and_then(Ns::href).as_deref() == Some(b"http://www.w3.org/1999/XSL/Transform");
        if n.kind() == Kind::Element && n.name() == b"key" && in_xslt {
            return true;
        }
        at = n.next();
    }
    false
}

/// libxml's errors from a parser libxslt opened, which php never
/// configured, are libxml's own text through php's generic handler.
fn routed(mut raised: Vec<Raised>) -> Vec<Raised> {
    for error in &mut raised {
        if error.route == Route::Parser {
            error.route = Route::Default;
        }
    }
    raised
}

// ---- the transformation

/// `php_xsl_apply_stylesheet`, then `finish` with the document made,
/// the caller's, or `None` where libxslt made none, and the stylesheet
/// to save it with. The stylesheet stays on the object and is held for
/// the whole span (ADR-0013), so a callback's nested transformation runs
/// with it and one that imports another leaves the old one's free to
/// the end of this one, saving included (#2319).
fn apply<R>(
    req: &mut Request,
    this: PhpObject,
    document: PhpObject,
    method: &str,
    finish: impl for<'h> FnOnce(&mut Request, ffi::Compiled<'h>, Option<Node>) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let exported = import_node(req, document);
    let Some((exported, doc)) = exported.and_then(|e| e.node().doc().map(|d| (e, d))) else {
        return Err(arg_type_error(req, 1, "be a valid XML node"));
    };
    let held = intern(req, this).and_then(|i| {
        let id = i.sheet.as_ref()?.id();
        let read = (
            i.has_keys,
            i.params.clone(),
            i.security,
            i.profiling.clone(),
        );
        Some((id, read))
    });
    let Some((id, (has_keys, params, security, profiling))) = held else {
        let message = format!(
            "XSLTProcessor::{method}() can only be called after a stylesheet has been imported"
        );
        return Err(throw_error(req, "Error", &message));
    };
    let profile = profiling.filter(|p| req.environment_mut().check_open_basedir(p, true).is_ok());
    let answer = id.held(req, |req, sheet| {
        if let Some(i) = intern(req, this) {
            i.transform_depth += 1;
        }
        let mut holder = NodeObject::default();
        let doc = if has_keys {
            let copy = doc.copy_doc(true).unwrap_or(doc);
            holder.adopt_document(req, copy);
            copy
        } else {
            holder.share(req, exported.document(), doc);
            doc
        };
        let state = State {
            doc,
            holder: &holder,
            params: &params,
            security,
            profile: profile.as_deref(),
        };
        let made = transform(req, this, sheet, &state);
        at(this).clean_node_list(req);
        holder.decrement_doc_ref(req);
        if let Some(i) = intern(req, this) {
            i.transform_depth -= 1;
        }
        finish(req, sheet, made?)
    });
    answer.unwrap_or_else(|| {
        let message = format!(
            "XSLTProcessor::{method}() can only be called after a stylesheet has been imported"
        );
        Err(throw_error(req, "Error", &message))
    })
}

/// What one transformation reads off its object.
struct State<'a> {
    /// The main document: the one given, or its copy.
    doc: Node,
    /// `intern->doc`, counting the main document for the transformation.
    holder: &'a NodeObject,
    params: &'a [(Vec<u8>, Vec<u8>)],
    security: i64,
    profile: Option<&'a [u8]>,
}

fn transform(
    req: &mut Request,
    this: PhpObject,
    sheet: ffi::Compiled<'_>,
    state: &State<'_>,
) -> Result<Option<Node>, Thrown> {
    let doc = state.doc;
    let Some(mut ctxt) = ffi::Transform::new(sheet, doc) else {
        return Ok(None);
    };
    for (name, value) in state.params {
        let (took, raised) = Raised::capture(|| ctxt.quote_param(name, value));
        Errors::route(req, raised);
        if !took {
            let shown = String::from_utf8_lossy(name);
            docref(
                req,
                Level::Warning,
                &format!("Could not apply parameter \"{shown}\""),
            );
            pending(req)?;
            break;
        }
    }
    let xinclude = Std.read_property(req, this, b"doXInclude", PropertyRead::Warning)?;
    let xinclude_on = truthy(req, &xinclude);
    xinclude.give_up(req);
    let xinclude_on = xinclude_on?;
    let depth = read_int(req, this, b"maxTemplateDepth")?;
    let vars = read_int(req, this, b"maxTemplateVars")?;
    ctxt.set_limits(xinclude_on, depth, vars);
    let secured = ctxt.secure(state.security);
    let registered = intern(req, this).map(|i| i.callbacks.registered());
    for (ns, name) in registered.unwrap_or_default() {
        ctxt.register(&ns, &name);
    }
    if !secured {
        let message =
            "Can't set libxslt security properties, not doing transformation for security reasons";
        docref(req, Level::Warning, message);
        return Ok(None);
    }
    let modern = state.holder.class_type() == ClassType::Modern;
    let mut proxy = |req: &mut Request, node: Node| {
        // `xsl_proxy_factory`: a node of another document is copied into
        // the main one, which libxslt frees the others before.
        let node = if node.doc() == Some(doc) {
            node
        } else {
            node.copy(Some(doc), 1).unwrap_or(node)
        };
        php_ext_dom::create_object_in(req, node, state.holder)
    };
    let at = at(this);
    let mut handler = |req: &mut Request, called, operands| {
        XPathCallbacks::call(req, at, called, operands, modern, &mut proxy)
    };
    let (result, raised, thrown) = ctxt.apply(req, state.profile, &mut handler);
    Errors::route(req, routed(raised));
    if let Some(thrown) = thrown {
        if let Some(result) = result {
            result.free_document();
        }
        return Err(thrown);
    }
    Ok(result)
}

/// `xsl_create_parameter_key`: the name, or `{namespace}name` where a
/// namespace is given.
fn parameter_key(req: &mut Request, namespace: &[u8], name: &[u8]) -> Result<Vec<u8>, Thrown> {
    if namespace.is_empty() {
        return Ok(name.to_vec());
    }
    if name.first() == Some(&b'{') {
        let tail = "not use clark notation when argument #1 ($namespace) is not empty";
        return Err(arg_value_error(req, 2, tail));
    }
    if name.first() != Some(&b':') && name.contains(&b':') {
        let tail = "not be a QName when argument #1 ($namespace) is not empty";
        return Err(arg_value_error(req, 2, tail));
    }
    Ok([b"{", namespace, b"}", name].concat())
}

fn set_param(req: &mut Request, this: PhpObject, key: Vec<u8>, value: Vec<u8>) {
    if let Some(i) = intern(req, this) {
        match i.params.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => i.params.push((key, value)),
        }
    }
}

/// Whether `class` descends from the root of `of`'s hierarchy,
/// `instanceof_function(ret_class, curce)`.
fn compatible(req: &Request, class: ClassId, of: ClassId) -> bool {
    let root = req.symbols().lineage(of).last().unwrap_or(of);
    req.symbols().lineage(class).any(|c| c == root)
}

/// Where `this` keeps its callbacks, which a transformation reaches at
/// each call it serves, so a callback's own registration on the
/// processor is the one the rest of the transformation sees (#2319).
fn at(this: PhpObject) -> CallbacksAt {
    CallbacksAt {
        object: this,
        reach: |req, object| intern(req, object).map(|i| &mut i.callbacks),
    }
}

/// The callbacks taken off the object for `f`, a registration that can
/// call PHP code, and put back after, in place of any laid meanwhile.
fn with_callbacks<R>(
    req: &mut Request,
    this: PhpObject,
    f: impl FnOnce(&mut Request, &mut XPathCallbacks) -> R,
) -> R {
    let mut callbacks = intern(req, this)
        .map(|i| std::mem::take(&mut i.callbacks))
        .unwrap_or_default();
    let answer = f(req, &mut callbacks);
    match intern(req, this) {
        Some(i) => std::mem::replace(&mut i.callbacks, callbacks).release(req),
        None => callbacks.release(req),
    }
    answer
}

/// `class XSLTProcessor` as `php_xsl.stub.php` declares it.
#[php_class(not_serializable, size = 192, beside(56), payload = Intern, handlers = HANDLERS)]
pub(crate) struct XSLTProcessor;

#[php_class]
impl XSLTProcessor {
    #[php(property)]
    const doXInclude: bool = false;
    #[php(property)]
    const cloneDocument: bool = false;
    #[php(property)]
    const maxTemplateDepth: i64;
    #[php(property)]
    const maxTemplateVars: i64;

    /// `importStylesheet`: a copy of the stylesheet's document compiled,
    /// as php compiles the clone it detaches, and `false` where it is no
    /// stylesheet.
    ///
    /// ```php
    /// $x = new XSLTProcessor;
    /// $s = new DOMDocument;
    /// $s->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><o/></xsl:template></xsl:stylesheet>');
    /// var_dump($x->importStylesheet($s), $x->importStylesheet($s->documentElement));
    /// $n = new DOMDocument;
    /// $n->loadXML('<not-a-stylesheet/>');
    /// var_dump($x->importStylesheet($n));
    /// try { $x->importStylesheet(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    ///
    /// Warning: XSLTProcessor::importStylesheet(): compilation error: file %s line 1 element not-a-stylesheet in %s on line 8
    ///
    /// Warning: XSLTProcessor::importStylesheet(): xsltParseStylesheetProcess : document is not a stylesheet in %s on line 8
    /// bool(false)
    /// XSLTProcessor::importStylesheet(): Argument #1 ($stylesheet) must be a valid XML node
    /// ```
    ///
    /// A stylesheet imported while the processor transforms, from a
    /// callback or from a destructor the transformation's node list runs
    /// as it is torn down, is refused, as php 8.5.12 refuses it
    /// (GH-23730); once the transformation ends, an import is taken:
    ///
    /// ```php
    /// class E extends DOMElement { function __destruct() { imp('destructor'); } }
    /// function imp($from) { try { $GLOBALS['p']->importStylesheet($GLOBALS['o']); echo "$from: imported\n"; } catch (Error $e) { echo "$from: ", $e->getMessage(), "\n"; } }
    /// function cb($n) { imp('callback'); return $n[0]; }
    /// $d = new DOMDocument; $d->registerNodeClass('DOMElement', 'E'); $d->loadXML('<r><i>a</i></r>');
    /// $s = new DOMDocument; $s->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="php:function(\'cb\', //i)"/></xsl:template></xsl:stylesheet>');
    /// $o = new DOMDocument; $o->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/">B</xsl:template></xsl:stylesheet>');
    /// $p = new XSLTProcessor; $p->registerPHPFunctions(); $p->importStylesheet($s);
    /// var_dump($p->transformToXml($d));
    /// imp('after');
    /// var_dump($p->transformToXml($d));
    /// ```
    /// ```expect
    /// callback: Cannot call XSLTProcessor::importStylesheet() while a transformation is in progress
    /// destructor: Cannot call XSLTProcessor::importStylesheet() while a transformation is in progress
    /// string(1) "a"
    /// after: imported
    /// string(1) "B"
    /// ```
    #[php(tentative)]
    fn importStylesheet(
        req: &mut Request,
        this: This<Self>,
        stylesheet: PhpObject,
    ) -> Result<bool, Thrown> {
        let this = this.object();
        if intern(req, this).is_some_and(|i| i.transform_depth > 0) {
            let message = "Cannot call XSLTProcessor::importStylesheet() while a transformation is in progress";
            return Err(throw_error(req, "Error", message));
        }
        let Some(exported) = import_node(req, stylesheet) else {
            return Err(arg_type_error(req, 1, "be a valid XML node"));
        };
        let node = exported.node();
        let doc = if node.kind().is_document() {
            Some(node)
        } else {
            node.doc()
        };
        let Some(doc) = doc else {
            return Err(arg_value_error(req, 1, "be part of a document"));
        };
        let Some(copy) = doc.copy_doc(true) else {
            return Err(arg_type_error(req, 1, "be a valid XML node"));
        };
        if exported.class_type() == ClassType::Modern {
            add_ns_defs(doc, copy, copy);
        }
        let (sheet, raised, thrown) = ffi::Stylesheet::parse(req, copy);
        let settled = Errors::settle(req, routed(raised), thrown);
        let Some(sheet) = sheet else {
            copy.free_document();
            settled?;
            return Ok(false);
        };
        settled?;
        let clone = Std.read_property(req, this, b"cloneDocument", PropertyRead::Warning)?;
        let cloned = truthy(req, &clone);
        clone.give_up(req);
        let has_keys = cloned? || sheet.doc().is_some_and(declares_keys);
        let sheet = ffi::Sheet::adopt(req, sheet);
        let replaced = match intern(req, this) {
            Some(i) => {
                i.has_keys = has_keys;
                i.sheet.replace(sheet)
            }
            None => Some(sheet),
        };
        if let Some(old) = replaced {
            old.close(req);
        }
        Ok(true)
    }

    /// `transformToDoc`: the document made, as an object of `$returnClass`, which
    /// descends from the root of the document's class, or of the class
    /// dom gives a document.
    ///
    /// ```php
    /// $s = new DOMDocument;
    /// $s->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:param name="p" select="\'none\'"/><xsl:template match="/"><o><xsl:value-of select="$p"/>-<xsl:value-of select="/r"/></o></xsl:template></xsl:stylesheet>');
    /// $d = new DOMDocument;
    /// $d->loadXML('<r>t</r>');
    /// $x = new XSLTProcessor;
    /// $x->importStylesheet($s);
    /// class D extends DOMDocument {}
    /// $o = $x->transformToDoc($d, D::class);
    /// var_dump(get_class($o), $o->saveXML($o->documentElement), get_class($x->transformToDoc($d->documentElement)));
    /// try { $x->transformToDoc($d, stdClass::class); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { (new XSLTProcessor)->transformToDoc($d); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "D"
    /// string(13) "<o>none-t</o>"
    /// string(11) "DOMDocument"
    /// XSLTProcessor::transformToDoc(): Argument #2 ($returnClass) must be a class name compatible with DOMDocument, stdClass given
    /// XSLTProcessor::transformToDoc() can only be called after a stylesheet has been imported
    /// ```
    ///
    /// The class named is `zend_lookup_class`, which asks the autoloaders.
    ///
    /// ```php
    /// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'Lazy') { class Lazy extends DOMDocument {} } });
    /// $xsl = new DOMDocument;
    /// $xsl->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:template match="/"><b/></xsl:template></xsl:stylesheet>');
    /// $p = new XSLTProcessor;
    /// $p->importStylesheet($xsl);
    /// $doc = new DOMDocument;
    /// $doc->loadXML('<a/>');
    /// echo get_class($p->transformToDoc($doc, 'Lazy')), "\n";
    /// ```
    /// ```expect
    /// load Lazy
    /// Lazy
    /// ```
    #[php(tentative)]
    fn transformToDoc(
        req: &mut Request,
        this: This<Self>,
        document: PhpObject,
        #[php(default = null)] returnClass: Option<PhpString>,
    ) -> Result<OrFalse<Owner<PhpObject>>, Thrown> {
        let this = this.object();
        let class = match returnClass {
            Some(name) => {
                let found = autoload(req, name.as_bytes())?;
                if found.is_none() {
                    let shown = String::from_utf8_lossy(name.as_bytes()).into_owned();
                    let tail = format!("be a valid class name or null, {shown} given");
                    return Err(arg_type_error(req, 2, &tail));
                }
                found
            }
            None => None,
        };
        let result = apply(req, this, document, "transformToDoc", |_, _, made| Ok(made))?;
        let Some(result) = result else {
            return Ok(OrFalse::False);
        };
        let Some(class) = class else {
            return Ok(
                match php_ext_dom::create_object(req, result, None)?.into_object() {
                    Ok(object) => OrFalse::Value(object),
                    Err(value) => {
                        value.give_up(req);
                        OrFalse::False
                    }
                },
            );
        };
        if !compatible(req, class, document.class()) {
            result.free_document();
            let (current, given) = (class_name(req, document.class()), class_name(req, class));
            let tail = format!("be a class name compatible with {current}, {given} given");
            return Err(arg_type_error(req, 2, &tail));
        }
        let object = new_object(req, class)?;
        if let Some(mut half) = php_ext_libxml::node_object(req, *object) {
            half.adopt_document(req, result);
            half.take_node(req, result);
        }
        Ok(OrFalse::Value(object))
    }

    /// `transformToUri`: the bytes written, `-1` where nothing was.
    ///
    /// ```php
    /// $s = new DOMDocument;
    /// $s->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:param name="p" select="\'none\'"/><xsl:template match="/"><o><xsl:value-of select="$p"/>-<xsl:value-of select="/r"/></o></xsl:template></xsl:stylesheet>');
    /// $d = new DOMDocument;
    /// $d->loadXML('<r>t</r>');
    /// $x = new XSLTProcessor;
    /// $x->importStylesheet($s);
    /// $f = tempnam(sys_get_temp_dir(), 'xsl');
    /// var_dump($x->transformToUri($d, $f), file_get_contents($f));
    /// unlink($f);
    /// ```
    /// ```expect
    /// int(36)
    /// string(36) "<?xml version="1.0"?>
    /// <o>none-t</o>
    /// "
    /// ```
    #[php(tentative)]
    fn transformToUri(
        req: &mut Request,
        this: This<Self>,
        document: PhpObject,
        uri: Path,
    ) -> Result<i64, Thrown> {
        let this = this.object();
        apply(req, this, document, "transformToUri", |req, sheet, made| {
            let Some(result) = made else {
                return Ok(-1);
            };
            let (n, raised, thrown) = sheet.save_to_file(req, uri.0.as_bytes(), result);
            result.free_document();
            Errors::settle(req, raised, thrown).map(|()| n)
        })
    }

    /// `transformToXml`: the document made, serialised as its `xsl:output` says,
    /// `null` where that is nothing and `false` where it failed.
    ///
    /// ```php
    /// $s = new DOMDocument;
    /// $s->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:param name="p" select="\'none\'"/><xsl:template match="/"><o><xsl:value-of select="$p"/>-<xsl:value-of select="/r"/></o></xsl:template></xsl:stylesheet>');
    /// $d = new DOMDocument;
    /// $d->loadXML('<r>t</r>');
    /// $x = new XSLTProcessor;
    /// $x->importStylesheet($s);
    /// var_dump($x->transformToXml($d));
    /// $e = new DOMDocument;
    /// $e->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:output method="text"/><xsl:template match="/"/></xsl:stylesheet>');
    /// $x->importStylesheet($e);
    /// var_dump($x->transformToXml($d));
    /// ```
    /// ```expect
    /// string(36) "<?xml version="1.0"?>
    /// <o>none-t</o>
    /// "
    /// NULL
    /// ```
    ///
    /// The stylesheet and the callbacks stay on the processor while it
    /// transforms, so a callback transforms with it again, and what it
    /// registers holds for the rest of the transformation and after
    /// (#2319).
    ///
    /// ```php
    /// $xsl = new DOMDocument;
    /// $xsl->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="php:function(\'inner\', string(/r))"/>|<xsl:value-of select="php:function(\'strrev\', string(/r))"/></xsl:template></xsl:stylesheet>');
    /// $doc = new DOMDocument;
    /// $doc->loadXML('<r>xy</r>');
    /// $p = new XSLTProcessor;
    /// $p->registerPHPFunctions(['inner']);
    /// $p->importStylesheet($xsl);
    /// $depth = 0;
    /// function inner($s) {
    ///     global $p, $doc, $depth;
    ///     if ($depth++) return "deep";
    ///     $p->registerPHPFunctions(['strrev']);
    ///     var_dump($p->transformToXml($doc));
    ///     return "in:$s";
    /// }
    /// var_dump($p->transformToXml($doc), $p->transformToXml($doc));
    /// ```
    /// ```expect
    /// string(7) "deep|yx"
    /// string(8) "in:xy|yx"
    /// string(7) "deep|yx"
    /// ```
    #[php(tentative)]
    fn transformToXml(
        req: &mut Request,
        this: This<Self>,
        document: PhpObject,
    ) -> Result<Option<OrFalse<Owner<PhpString>>>, Thrown> {
        let this = this.object();
        let saved = apply(req, this, document, "transformToXml", |_, sheet, made| {
            Ok(made.and_then(|r| {
                let saved = sheet.save_to_string(r);
                r.free_document();
                saved
            }))
        })?;
        match saved {
            None => Ok(Some(OrFalse::False)),
            Some(bytes) if bytes.is_empty() => Ok(None),
            Some(bytes) => Ok(Some(OrFalse::Value(PhpString::new(req, &bytes)?))),
        }
    }

    /// `setParameter`: one parameter, or each of an array's, kept by its
    /// key in Clark notation.
    ///
    /// ```php
    /// $s = new DOMDocument;
    /// $s->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:param name="p" select="\'none\'"/><xsl:template match="/"><o><xsl:value-of select="$p"/>-<xsl:value-of select="/r"/></o></xsl:template></xsl:stylesheet>');
    /// $d = new DOMDocument;
    /// $d->loadXML('<r>t</r>');
    /// $x = new XSLTProcessor;
    /// $x->importStylesheet($s);
    /// var_dump($x->setParameter('', 'p', 'one'), $x->transformToXml($d));
    /// var_dump($x->setParameter('urn:a', ['p' => 'two', 'q' => 3]), $x->getParameter('urn:a', 'q'));
    /// try { $x->setParameter('urn:a', 'b:c', 'v'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { $x->setParameter('', 'p'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { $x->setParameter('', [1 => 'v']); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// string(35) "<?xml version="1.0"?>
    /// <o>one-t</o>
    /// "
    /// bool(true)
    /// string(1) "3"
    /// XSLTProcessor::setParameter(): Argument #2 ($name) must not be a QName when argument #1 ($namespace) is not empty
    /// XSLTProcessor::setParameter(): Argument #3 ($value) cannot be null when argument #2 ($name) is a string
    /// XSLTProcessor::setParameter(): Argument #2 ($name) must contain only string keys
    /// ```
    #[php(tentative)]
    fn setParameter(
        req: &mut Request,
        this: This<Self>,
        namespace: Path,
        name: Union<PhpArray, PhpString>,
        #[php(default = null)] value: Option<Path>,
    ) -> Result<bool, Thrown> {
        let this = this.object();
        let namespace = namespace.0.as_bytes();
        let name = match name {
            Union::A(array) => {
                if value.is_some() {
                    let tail = "be null when argument #2 ($name) is an array";
                    return Err(arg_value_error(req, 3, tail));
                }
                array
            }
            Union::B(name) => {
                let Some(value) = value else {
                    let tail = "cannot be null when argument #2 ($name) is a string";
                    return Err(argument_error(req, "ValueError", 3, tail));
                };
                if name.as_bytes().contains(&0) {
                    return Err(arg_value_error(req, 2, "not contain any null bytes"));
                }
                let key = parameter_key(req, namespace, name.as_bytes())?;
                set_param(req, this, key, value.0.as_bytes().to_vec());
                return Ok(true);
            }
        };
        // `ZEND_HASH_FOREACH_STR_KEY_VAL`, holding nothing: a conversion
        // may serve `__toString`.
        walk::foreach(req, name, Hold::None, |req, row| -> walk::Step {
            let Key::Str(key) = row.key().clone() else {
                return Err(arg_type_error(req, 2, "contain only string keys"));
            };
            if key.as_bytes().contains(&0) {
                let tail = "not contain keys with any null bytes";
                return Err(arg_value_error(req, 3, tail));
            }
            let key = parameter_key(req, namespace, key.as_bytes())?;
            let entry = with_string(req, row.value(), <[u8]>::to_vec)?;
            if entry.contains(&0) {
                let tail = "not contain values with any null bytes";
                return Err(arg_value_error(req, 3, tail));
            }
            set_param(req, this, key, entry);
            Ok(std::ops::ControlFlow::Continue(()))
        })?;
        Ok(true)
    }

    /// `getParameter`: the value kept, `false` for none.
    ///
    /// ```php
    /// $x = new XSLTProcessor;
    /// $x->setParameter('', 'p', 'v');
    /// var_dump($x->getParameter('', 'p'), $x->getParameter('', 'q'));
    /// try { $x->getParameter('urn:a', '{urn:a}p'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "v"
    /// bool(false)
    /// XSLTProcessor::getParameter(): Argument #2 ($name) must not use clark notation when argument #1 ($namespace) is not empty
    /// ```
    #[php(tentative)]
    fn getParameter(
        req: &mut Request,
        this: This<Self>,
        namespace: Path,
        name: Path,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let key = parameter_key(req, namespace.0.as_bytes(), name.0.as_bytes())?;
        let found = intern(req, this.object()).and_then(|i| {
            i.params
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        });
        match found {
            Some(v) => Ok(OrFalse::Value(PhpString::new(req, &v)?)),
            None => Ok(OrFalse::False),
        }
    }

    /// `removeParameter`: whether one was kept.
    ///
    /// ```php
    /// $x = new XSLTProcessor;
    /// $x->setParameter('', 'p', 'v');
    /// var_dump($x->removeParameter('', 'p'), $x->removeParameter('', 'p'), $x->getParameter('', 'p'));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn removeParameter(
        req: &mut Request,
        this: This<Self>,
        namespace: Path,
        name: Path,
    ) -> Result<bool, Thrown> {
        let key = parameter_key(req, namespace.0.as_bytes(), name.0.as_bytes())?;
        let Some(i) = intern(req, this.object()) else {
            return Ok(false);
        };
        let before = i.params.len();
        i.params.retain(|(k, _)| *k != key);
        Ok(i.params.len() != before)
    }

    /// `hasExsltSupport`: libexslt is linked.
    ///
    /// ```php
    /// $x = new XSLTProcessor;
    /// var_dump($x->hasExsltSupport());
    /// $s = new DOMDocument;
    /// $s->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:str="http://exslt.org/strings" extension-element-prefixes="str"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="str:padding(3, \'ab\')"/></xsl:template></xsl:stylesheet>');
    /// $x->importStylesheet($s);
    /// var_dump($x->transformToXml(new DOMDocument('1.0')));
    /// ```
    /// ```expect
    /// bool(true)
    /// string(3) "aba"
    /// ```
    #[php(tentative)]
    fn hasExsltSupport(this: This<Self>) -> bool {
        let _ = this;
        true
    }

    /// `registerPHPFunctions`: every php function, or the ones named.
    ///
    /// ```php
    /// $s = new DOMDocument;
    /// $s->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:php="http://php.net/xsl"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="php:functionString(\'strtoupper\', /r)"/></xsl:template></xsl:stylesheet>');
    /// $d = new DOMDocument;
    /// $d->loadXML('<r>t</r>');
    /// $x = new XSLTProcessor;
    /// $x->importStylesheet($s);
    /// $x->registerPHPFunctions('strtoupper');
    /// var_dump($x->transformToXml($d));
    /// $x->registerPHPFunctions(['strrev']);
    /// try { $x->registerPHPFunctions(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// $y = new XSLTProcessor;
    /// $y->importStylesheet($s);
    /// try { $y->registerPHPFunctions(['strtoupper', 'no_such_function']); } catch (TypeError $e) { echo get_class($e), "\n"; }
    /// var_dump($y->transformToXml($d));
    /// ```
    /// ```expect
    /// string(1) "T"
    /// XSLTProcessor::registerPHPFunctions(): Argument #1 ($functions) must be a valid callback name
    /// TypeError
    /// string(1) "T"
    /// ```
    #[php(tentative)]
    fn registerPHPFunctions(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] functions: Option<Union<PhpArray, PhpString>>,
    ) -> Result<(), Thrown> {
        let restrict = functions.map(|given| match given {
            Union::A(array) => Value::array(array),
            Union::B(name) => Value::string(name),
        });
        with_callbacks(req, this.object(), |req, c| c.register_php(req, restrict))
    }

    /// `registerPHPFunctionNS`: `$callable` as `$name` in `$namespaceURI`.
    ///
    /// ```php
    /// $s = new DOMDocument;
    /// $s->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform" xmlns:f="urn:f"><xsl:output method="text"/><xsl:template match="/"><xsl:value-of select="f:up(/r)"/></xsl:template></xsl:stylesheet>');
    /// $d = new DOMDocument;
    /// $d->loadXML('<r>t</r>');
    /// $x = new XSLTProcessor;
    /// $x->registerPHPFunctionNS('urn:f', 'up', fn($n) => strtoupper($n[0]->textContent));
    /// $x->importStylesheet($s);
    /// var_dump($x->transformToXml($d));
    /// try { $x->registerPHPFunctionNS('http://php.net/xsl', 'up', 'strlen'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { $x->registerPHPFunctionNS('urn:f', 'a:b', 'strlen'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "T"
    /// XSLTProcessor::registerPHPFunctionNS(): Argument #1 ($namespaceURI) must not be "http://php.net/xsl" because it is reserved by PHP
    /// XSLTProcessor::registerPHPFunctionNS(): Argument #2 ($name) must be a valid callback name
    /// ```
    fn registerPHPFunctionNS(
        req: &mut Request,
        this: This<Self>,
        namespaceURI: Path,
        name: Path,
        callable: Raw<Callable>,
    ) -> Result<(), Thrown> {
        callable.hold(req)?;
        let ns = namespaceURI.0.as_bytes();
        if ns == b"http://php.net/xsl" {
            let tail = "not be \"http://php.net/xsl\" because it is reserved by PHP";
            return Err(arg_value_error(req, 1, tail));
        }
        let value = callable.value();
        with_callbacks(req, this.object(), |req, c| {
            c.register_ns(req, ns, name.0.as_bytes(), &value)
        })
    }

    /// `setProfiling`: the path the next transformations write their
    /// profile to, none for `null`.
    ///
    /// ```php
    /// $s = new DOMDocument;
    /// $s->loadXML('<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform"><xsl:param name="p" select="\'none\'"/><xsl:template match="/"><o><xsl:value-of select="$p"/>-<xsl:value-of select="/r"/></o></xsl:template></xsl:stylesheet>');
    /// $d = new DOMDocument;
    /// $d->loadXML('<r>t</r>');
    /// $x = new XSLTProcessor;
    /// $x->importStylesheet($s);
    /// $f = tempnam(sys_get_temp_dir(), 'xsl');
    /// var_dump($x->setProfiling($f));
    /// $x->transformToXml($d);
    /// var_dump(str_contains(file_get_contents($f), 'number'), $x->setProfiling(null));
    /// unlink($f);
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn setProfiling(req: &mut Request, this: This<Self>, filename: Option<Path>) -> True {
        if let Some(i) = intern(req, this.object()) {
            i.profiling = filename.map(|f| Box::from(f.0.as_bytes()));
        }
        True
    }

    /// `setSecurityPrefs`: the preferences before.
    ///
    /// ```php
    /// $x = new XSLTProcessor;
    /// var_dump($x->setSecurityPrefs(XSL_SECPREF_NONE), $x->setSecurityPrefs(XSL_SECPREF_READ_FILE));
    /// ```
    /// ```expect
    /// int(44)
    /// int(0)
    /// ```
    #[php(tentative)]
    fn setSecurityPrefs(req: &mut Request, this: This<Self>, preferences: i64) -> i64 {
        intern(req, this.object()).map_or(0, |i| std::mem::replace(&mut i.security, preferences))
    }

    /// `getSecurityPrefs`.
    ///
    /// ```php
    /// $x = new XSLTProcessor;
    /// var_dump($x->getSecurityPrefs() === XSL_SECPREF_DEFAULT);
    /// $x->setSecurityPrefs(XSL_SECPREF_WRITE_FILE);
    /// var_dump($x->getSecurityPrefs());
    /// ```
    /// ```expect
    /// bool(true)
    /// int(4)
    /// ```
    #[php(tentative)]
    fn getSecurityPrefs(req: &mut Request, this: This<Self>) -> i64 {
        intern(req, this.object()).map_or(0, |i| i.security)
    }
}
