//! The `simplexml` extension: `php-src/ext/simplexml` as one crate under
//! `ext/` (ADR-0004), over the libxml2 `ext/libxml` binds, reached through
//! its safe types (design §4.10, §7.2), with no `src/ffi/` of its own. Its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/simplexml/tests`, through `xtask phpt --subset`.
//!
//! Every object carries one payload, [`sxe::Sxe`], php's `php_sxe_object`:
//! the libxml node object that holds a node without standing for it (so
//! dom still answers a node of its own for it) and counts its document,
//! and the walk it makes from there. Properties and dimensions are the
//! handlers of [`sxe::HANDLERS`]; the objects export their node to
//! `dom_import_simplexml` through `ext/libxml`'s table of exports, and
//! `simplexml_import_dom` imports any other extension's through it.
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

mod sxe;

use php_ext::abi::{
    Dependency, DependencyKind, Extension, ExtensionBuilder, FromArgument, Instance, OrFalse, Path,
    Raw, This, Union,
};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{autoload, lookup_class, new_object};
use php_ext::coerce::coerce_param;
use php_ext::decl::Type as DeclType;
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, argument_error, docref, throw, throw_error,
};
use php_ext::request::Request;
use php_ext::values::{ClassId, Layout, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};
use php_ext_libxml::ffi::{
    Answer, ClassType, Exported, Item, Kind, Name, Node, Parser, Returned, Scope, Source, XPath,
};
use php_ext_libxml::{Errors, import_node, register_export};

use sxe::{Ns, Sxe, View, Walk, c_name, sxe, sxe_mut, wrap};

/// Core's interfaces and SPL's `RecursiveIterator`, named for `implements`.
#[php_class(extern)]
pub(crate) struct Stringable;

#[php_class(extern)]
pub(crate) struct Countable;

#[php_class(extern)]
pub(crate) struct RecursiveIterator;

/// The extension value (design §7.1).
///
/// ```php
/// var_dump(extension_loaded('simplexml'));
/// print_r((new ReflectionExtension('SimpleXML'))->getDependencies());
/// echo implode(',', (new ReflectionClass('SimpleXMLElement'))->getInterfaceNames()), "\n";
/// echo get_parent_class(new SimpleXMLIterator('<a/>')), "\n";
/// ```
/// ```expect
/// bool(true)
/// Array
/// (
///     [libxml] => Required
///     [spl] => Required
/// )
/// Stringable,Countable,RecursiveIterator,Traversable,Iterator
/// SimpleXMLElement
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            simplexml_load_file::declared(),
            simplexml_load_string::declared(),
            simplexml_import_dom::declared(),
        ])
        .dependencies(vec![
            dependency("libxml", DependencyKind::Required),
            dependency("spl", DependencyKind::Required),
        ])
        .class::<SimpleXMLElement>()
        .class::<SimpleXMLIterator>()
        .register(
            register_export(b"SimpleXMLElement", export_node).with_node_object(sxe::libxml_half),
        )
        .info(simplexml_info)
        .build("SimpleXML")
}

fn dependency(name: &str, kind: DependencyKind) -> Dependency {
    Dependency {
        name: String::from(name),
        kind,
        rel: None,
        version: None,
    }
}

/// `PHP_MINFO_FUNCTION(simplexml)`.
///
/// ```php
/// (new ReflectionExtension('SimpleXML'))->info();
/// ```
/// ```expect
///
/// SimpleXML
///
/// SimpleXML support => enabled
/// Schema support => enabled
/// ```
fn simplexml_info(req: &mut Request) {
    req.extensions_mut().info_table(&[
        &["SimpleXML support", "enabled"],
        &["Schema support", "enabled"],
    ]);
}

/// `simplexml_export_node`: the first node of the object's walk, what
/// `php_libxml_import_node` answers for a `SimpleXMLElement`.
fn export_node(req: &mut Request, object: PhpObject) -> Option<Exported> {
    let view = View::of(req, object);
    let node = view.first(view.node?)?;
    Some(Exported::of(node, &sxe(req, object)?.obj))
}

// ---- the functions

/// `Z_PARAM_CLASS_OR_NULL` of a class derived from `SimpleXMLElement`,
/// `null` being `SimpleXMLElement` itself.
///
/// The class named is `zend_lookup_class`, which asks the autoloaders.
///
/// ```php
/// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'Lazy') { class Lazy extends SimpleXMLElement {} } });
/// echo get_class(simplexml_load_string('<a/>', 'Lazy')), "\n";
/// ```
/// ```expect
/// load Lazy
/// Lazy
/// ```
fn class_argument(req: &mut Request, given: Value, position: u32) -> Result<ClassId, Thrown> {
    let base = element_class(req)?;
    if matches!(given.as_ref(), ValueRef::Null | ValueRef::Undef) {
        return Ok(base);
    }
    let name = coerce_param(req, &given, &DeclType::String, position)?;
    let name = name
        .as_php_string()
        .map_or_else(Vec::new, |s| s.as_bytes().to_vec());
    let found = autoload(req, &name)?;
    match found {
        Some(class) if derives(req, class, base) => Ok(class),
        _ => {
            let shown = String::from_utf8_lossy(&name).into_owned();
            Err(arg_type_error(
                req,
                position,
                &format!("be a class name derived from SimpleXMLElement or null, {shown} given"),
            ))
        }
    }
}

/// `ce_SimpleXMLElement`.
fn element_class(req: &mut Request) -> Result<ClassId, Thrown> {
    match lookup_class(req, b"SimpleXMLElement")? {
        Some(class) => Ok(class),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: SimpleXMLElement is not linked",
        )),
    }
}

/// Whether `class` is `base` or extends it.
fn derives(req: &mut Request, class: ClassId, base: ClassId) -> bool {
    req.symbols().lineage(class).any(|at| at == base)
}

/// `php_libxml_increment_doc_ref(sxe, docp)` and the root element taken.
fn adopt(req: &mut Request, object: PhpObject, doc: Node, ns: Option<Ns>) {
    if let Some(s) = sxe_mut(req, object) {
        s.ns = ns;
    }
    if let Some(mut obj) = sxe::libxml_half(req, object) {
        obj.adopt_document(req, doc);
        if let Some(root) = doc.root() {
            obj.hold_node(req, root);
        }
    }
}

/// `ZEND_LONG_EXCEEDS_INT`.
fn exceeds_int(n: i64) -> bool {
    i32::try_from(n).is_err()
}

/// The parse the functions and the constructor share, what libxml raised
/// routed on.
fn parse(req: &mut Request, source: Source<'_>, options: i64) -> Result<Option<Node>, Thrown> {
    let options = i32::try_from(options).unwrap_or_default();
    let (doc, raised, thrown) = Parser::read(req, source, options);
    Errors::settle(req, raised, thrown)?;
    Ok(doc)
}

/// `simplexml_load_string` and `simplexml_load_file` after their
/// arguments: the object of `class` for what the parse made.
fn load(
    req: &mut Request,
    source: Source<'_>,
    class: ClassId,
    options: i64,
    ns: Option<Ns>,
) -> Result<OrFalse<Owner<Instance<SimpleXMLElement>>>, Thrown> {
    if exceeds_int(options) {
        return Err(argument_error(req, "ValueError", 3, "is too large"));
    }
    let Some(doc) = parse(req, source, options)? else {
        return Ok(OrFalse::False);
    };
    let made = new_object(req, class)?;
    adopt(req, *made, doc, ns);
    Ok(OrFalse::Value(made.into_instance()))
}

/// `simplexml_load_file(string $filename, ?string $class_name = SimpleXMLElement::class, int $options = 0, string $namespace_or_prefix = "", bool $is_prefix = false): SimpleXMLElement|false`.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'sx');
/// file_put_contents($f, '<r><a>1</a></r>');
/// $s = simplexml_load_file($f);
/// var_dump(get_class($s), (string) $s->a);
/// var_dump(@simplexml_load_file($f . '.missing'));
/// unlink($f);
/// ```
/// ```expect
/// string(16) "SimpleXMLElement"
/// string(1) "1"
/// bool(false)
/// ```
#[php_function]
fn simplexml_load_file(
    req: &mut Request,
    filename: Path,
    #[php(default = SimpleXMLElement::class)] class_name: Raw<Option<PhpString>>,
    #[php(default = 0)] options: i64,
    #[php(default = "")] namespace_or_prefix: PhpString,
    #[php(default = false)] is_prefix: bool,
) -> Result<OrFalse<Owner<Instance<SimpleXMLElement>>>, Thrown> {
    let class = class_argument(req, class_name.value(), 2)?;
    let ns = Ns::of(Some(namespace_or_prefix.as_bytes()), is_prefix);
    load(req, Source::File(filename.0.as_bytes()), class, options, ns)
}

/// `simplexml_load_string(string $data, ?string $class_name = SimpleXMLElement::class, int $options = 0, string $namespace_or_prefix = "", bool $is_prefix = false): SimpleXMLElement|false`.
///
/// ```php
/// class X extends SimpleXMLElement {}
/// $s = simplexml_load_string('<r a="1"><b>t</b></r>', 'X');
/// var_dump(get_class($s), (string) $s->b, get_class($s->b));
/// var_dump(simplexml_load_string('<r'));
/// try { simplexml_load_string('<r/>', 'stdClass'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "X"
/// string(1) "t"
/// string(1) "X"
///
/// Warning: simplexml_load_string(): Entity: line 1: parser error : Couldn't find end of Start Tag r line 1 in %s on line 5
///
/// Warning: simplexml_load_string(): <r in %s on line 5
///
/// Warning: simplexml_load_string():   ^ in %s on line 5
/// bool(false)
/// simplexml_load_string(): Argument #2 ($class_name) must be a class name derived from SimpleXMLElement or null, stdClass given
/// ```
#[php_function]
fn simplexml_load_string(
    req: &mut Request,
    data: PhpString,
    #[php(default = SimpleXMLElement::class)] class_name: Raw<Option<PhpString>>,
    #[php(default = 0)] options: i64,
    #[php(default = "")] namespace_or_prefix: PhpString,
    #[php(default = false)] is_prefix: bool,
) -> Result<OrFalse<Owner<Instance<SimpleXMLElement>>>, Thrown> {
    let class = class_argument(req, class_name.value(), 2)?;
    let ns = Ns::of(Some(namespace_or_prefix.as_bytes()), is_prefix);
    load(req, Source::Memory(data.as_bytes()), class, options, ns)
}

/// `simplexml_import_dom(object $node, ?string $class_name = SimpleXMLElement::class): ?SimpleXMLElement`:
/// the element any XML extension's object stands for, through libxml's
/// table of exports, its document shared; a document answers as its root.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a>1</a></r>');
/// $s = simplexml_import_dom($d);
/// $s->a = 2;
/// echo $d->saveXML($d->documentElement), "\n";
/// var_dump(simplexml_import_dom($d->createTextNode('t')));
/// try { simplexml_import_dom(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// <r><a>2</a></r>
///
/// Warning: simplexml_import_dom(): Invalid Nodetype to import in %s on line 7
/// NULL
/// simplexml_import_dom(): Argument #1 ($node) must be a valid XML node
/// ```
#[php_function]
fn simplexml_import_dom(
    req: &mut Request,
    node: PhpObject,
    #[php(default = SimpleXMLElement::class)] class_name: Raw<Option<PhpString>>,
) -> Result<Option<Owner<Instance<SimpleXMLElement>>>, Thrown> {
    let class = class_argument(req, class_name.value(), 2)?;
    let Some(exported) = import_node(req, node) else {
        return Err(arg_type_error(req, 1, "be a valid XML node"));
    };
    let mut target = Some(exported.node());
    let Some(doc) = exported.node().doc() else {
        docref(
            req,
            Level::Warning,
            "Imported Node must have associated Document",
        );
        return Ok(None);
    };
    if exported.node().kind().is_document() {
        target = exported.node().root();
    }
    let Some(target) = target.filter(|t| t.kind() == Kind::Element) else {
        docref(req, Level::Warning, "Invalid Nodetype to import");
        return Ok(None);
    };
    let made = new_object(req, class)?;
    if let Some(mut obj) = sxe::libxml_half(req, *made) {
        obj.share(req, exported.document(), doc);
        obj.hold_node(req, target);
    }
    Ok(Some(made.into_instance()))
}

// ---- the classes

/// `SimpleXMLElement::xpath`'s answer, `array|null|false`.
type Found = Option<OrFalse<Owner<PhpArray>>>;

/// A `SimpleXMLElement` value handed back under its declared type.
type Element = Raw<Instance<SimpleXMLElement>>;

/// `class SimpleXMLElement implements Stringable, Countable,
/// RecursiveIterator`, php's `@not-serializable` one, reached through
/// [`sxe::HANDLERS`].
///
/// `sxe_object_new` allocates a `php_sxe_object`, which
/// `memory_get_usage()` counts (design §5.13):
///
/// ```php
/// foreach ([false, true] as $shown) foreach (['SimpleXMLElement'] as $c) {
///     $r = new ReflectionClass($c);
///     $m = memory_get_usage();
///     $o = $r->newInstanceWithoutConstructor();
///     $used = memory_get_usage() - $m;
///     $o = null;
///     if ($shown) echo $c, ' ', $used, "\n";
/// }
/// ```
/// ```expect
/// SimpleXMLElement 160
/// ```
#[php_class(
    not_serializable,
    implements(Stringable, Countable, RecursiveIterator),
    size = 152, payload = Sxe,
    handlers = sxe::HANDLERS
)]
pub(crate) struct SimpleXMLElement;

#[php_class]
impl SimpleXMLElement {
    /// `xpath(string $expression): array|null|false`: the nodes the
    /// expression selects from the object's node, the node's declarations
    /// in scope; a text node answers as its parent and an attribute as its
    /// parent's attribute list of that name.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r xmlns:p="urn:p"><a x="1">t</a><p:b/><!--c--></r>');
    /// foreach ($s->xpath('//a | //p:b | //a/@x | //a/text()') as $n) echo get_class($n), ' ', $n->getName(), "\n";
    /// var_dump($s->xpath('count(//a)'));
    /// $s->registerXPathNamespace('q', 'urn:p');
    /// var_dump(count($s->xpath('//q:b')), $s->attributes()->xpath('/'));
    /// ```
    /// ```expect
    /// SimpleXMLElement a
    /// SimpleXMLElement x
    /// SimpleXMLElement a
    /// SimpleXMLElement b
    ///
    /// Warning: SimpleXMLElement::xpath(): XPath expression must return a node set, number returned in %s on line 4
    /// bool(false)
    /// int(1)
    /// NULL
    /// ```
    ///
    /// The answer is `array_init_size` at the node set's size and
    /// `zend_hash_real_init_packed` before a node is added, so a set with
    /// no node is a packed table laid down, and nine nodes write into the
    /// sixteen slots taken from the start.
    ///
    /// ```php
    /// $x = simplexml_load_string('<a><b/></a>'); $r = $x->xpath('//c'); unset($r);
    /// $m = memory_get_usage(); $r = $x->xpath('//c'); var_dump(memory_get_usage() - $m); debug_zval_dump($r);
    /// $y = simplexml_load_string('<a>' . str_repeat('<b/>', 9) . '</a>'); $r = $y->xpath('//b'); unset($r);
    /// memory_reset_peak_usage(); $m = memory_get_usage(); $r = $y->xpath('//b');
    /// var_dump(memory_get_peak_usage() - $m, memory_get_usage() - $m);
    /// ```
    /// ```expect
    /// int(216)
    /// array(0) packed refcount(2){
    /// }
    /// int(2032)
    /// int(2032)
    /// ```
    #[php(tentative)]
    fn xpath(req: &mut Request, this: This<Self>, expression: PhpString) -> Result<Found, Thrown> {
        let this = this.object();
        let view = View::of(req, this);
        if matches!(view.walk, Walk::Attributes(_)) {
            return Ok(None);
        }
        let base = view.node(req)?;
        let Some(node) = view.first(base) else {
            return Ok(None);
        };
        let Some(context) = evaluator(req, this) else {
            return Ok(Some(OrFalse::False));
        };
        let mut unregistered = |_: &mut Request, _, _| Ok(Returned::Sentinel);
        let (answer, raised, thrown) = context.eval_scoped(
            req,
            expression.as_bytes(),
            Some(node),
            Scope::InScope,
            &mut unregistered,
        );
        if let Some(s) = sxe_mut(req, this) {
            s.xpath = Some(context);
        }
        Errors::settle(req, raised, thrown)?;
        let items = match answer {
            None => return Ok(Some(OrFalse::False)),
            Some(Answer::NodeSet(items)) => items,
            Some(other) => {
                let kind = match other {
                    Answer::Bool(_) => "bool",
                    Answer::Number(_) => "number",
                    Answer::Str(_) => "string",
                    Answer::NodeSet(_) | Answer::Other => "undefined",
                };
                docref(
                    req,
                    Level::Warning,
                    &format!("XPath expression must return a node set, {kind} returned"),
                );
                return Ok(Some(OrFalse::False));
            }
        };
        let size = u32::try_from(items.len()).unwrap_or(u32::MAX);
        let mut out = PhpArray::sized(req, size, Layout::Packed)?;
        for item in items {
            let Item::Node(n) = item else { continue };
            let (target, walk, ns) = match n.kind() {
                Kind::Text => (n.parent(), Walk::Node, None),
                Kind::Attribute => {
                    let href = n.ns().and_then(php_ext_libxml::ffi::Ns::href);
                    let ns = Ns::of(href.as_deref(), false);
                    (n.parent(), Walk::Attributes(Some(n.name())), ns)
                }
                Kind::Element | Kind::Pi | Kind::Comment => (Some(n), Walk::Node, None),
                _ => continue,
            };
            if let Some(target) = target {
                let made = wrap(req, this, target, walk, ns)?;
                out.append(req, made.into_value())?;
            }
        }
        Ok(Some(OrFalse::Value(out)))
    }

    /// `registerXPathNamespace(string $prefix, string $namespace): bool`,
    /// kept on the object's evaluator for its later queries.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r xmlns="urn:a"><b>t</b></r>');
    /// var_dump($s->registerXPathNamespace('a', 'urn:a'), (string) $s->xpath('//a:b')[0]);
    /// ```
    /// ```expect
    /// bool(true)
    /// string(1) "t"
    /// ```
    #[php(tentative)]
    fn registerXPathNamespace(
        req: &mut Request,
        this: This<Self>,
        prefix: PhpString,
        namespace: PhpString,
    ) -> Result<bool, Thrown> {
        let this = this.object();
        if sxe(req, this).is_none_or(|s| !s.obj.has_document()) {
            return Err(throw_error(
                req,
                "Error",
                "SimpleXMLElement is not properly initialized",
            ));
        }
        let Some(context) = evaluator(req, this) else {
            return Ok(false);
        };
        let taken = context.register_ns(prefix.as_bytes(), namespace.as_bytes());
        if let Some(s) = sxe_mut(req, this) {
            s.xpath = Some(context);
        }
        Ok(taken)
    }

    /// `asXML(?string $filename = null): string|bool`: the document where
    /// the node is its root, the node alone otherwise, as a string or
    /// written to `$filename`.
    ///
    /// ```php
    /// $s = simplexml_load_string('<?xml version="1.0"?><r><a>t</a></r>');
    /// echo $s->asXML(), $s->a->asXML(), "\n";
    /// $f = tempnam(sys_get_temp_dir(), 'sx');
    /// var_dump($s->a->saveXML($f), file_get_contents($f));
    /// unlink($f);
    /// ```
    /// ```expect
    /// <?xml version="1.0"?>
    /// <r><a>t</a></r>
    /// <a>t</a>
    /// bool(true)
    /// string(8) "<a>t</a>"
    /// ```
    #[php(tentative)]
    fn asXML(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] filename: Option<Path>,
    ) -> Result<Union<Owner<PhpString>, bool>, Thrown> {
        as_xml(req, this.object(), filename)
    }

    #[php(tentative, implementation = SimpleXMLElement::asXML)]
    fn saveXML(
        this: This<Self>,
        #[php(default = null)] filename: Option<Path>,
    ) -> Union<PhpString, bool>;

    /// `getNamespaces(bool $recursive = false): array`: the namespaces the
    /// node and its attributes use, its descendants' too where
    /// `$recursive`.
    ///
    /// ```php
    /// $s = simplexml_load_string('<p:r xmlns:p="urn:p" xmlns:q="urn:q" q:a="1"><c xmlns="urn:c"/></p:r>');
    /// var_dump($s->getNamespaces(), $s->getNamespaces(true));
    /// ```
    /// ```expect
    /// array(2) {
    ///   ["p"]=>
    ///   string(5) "urn:p"
    ///   ["q"]=>
    ///   string(5) "urn:q"
    /// }
    /// array(3) {
    ///   ["p"]=>
    ///   string(5) "urn:p"
    ///   ["q"]=>
    ///   string(5) "urn:q"
    ///   [""]=>
    ///   string(5) "urn:c"
    /// }
    /// ```
    #[php(tentative)]
    fn getNamespaces(
        req: &mut Request,
        this: This<Self>,
        #[php(default = false)] recursive: bool,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let view = View::of(req, this.object());
        let base = view.node(req)?;
        let mut found = Vec::new();
        if let Some(node) = view.first(base) {
            match node.kind() {
                Kind::Element => used_namespaces(node, recursive, &mut found),
                Kind::Attribute => {
                    if let Some(ns) = node.ns() {
                        add_namespace(&mut found, ns.prefix(), ns.href());
                    }
                }
                _ => {}
            }
        }
        namespace_array(req, found)
    }

    /// `getDocNamespaces(bool $recursive = false, bool $fromRoot = true): array|false`:
    /// the declarations on the root, or on the node, and on their
    /// descendants where `$recursive`; a modern document's `xmlns`
    /// attributes count as declarations.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r xmlns:a="urn:a"><c xmlns:b="urn:b"/></r>');
    /// var_dump($s->getDocNamespaces(), $s->getDocNamespaces(true), $s->c->getDocNamespaces(false, false));
    /// ```
    /// ```expect
    /// array(1) {
    ///   ["a"]=>
    ///   string(5) "urn:a"
    /// }
    /// array(2) {
    ///   ["a"]=>
    ///   string(5) "urn:a"
    ///   ["b"]=>
    ///   string(5) "urn:b"
    /// }
    /// array(1) {
    ///   ["a"]=>
    ///   string(5) "urn:a"
    /// }
    /// ```
    #[php(tentative)]
    fn getDocNamespaces(
        req: &mut Request,
        this: This<Self>,
        #[php(default = false)] recursive: bool,
        #[php(default = true)] fromRoot: bool,
    ) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
        let this = this.object();
        let node = if fromRoot {
            let Some(doc) = sxe(req, this).and_then(|s| s.obj.document_node()) else {
                return Err(throw_error(
                    req,
                    "Error",
                    "SimpleXMLElement is not properly initialized",
                ));
            };
            doc.root()
        } else {
            Some(View::of(req, this).node(req)?)
        };
        let Some(node) = node else {
            return Ok(OrFalse::False);
        };
        let modern = sxe(req, this).is_some_and(|s| s.obj.class_type() == ClassType::Modern);
        let mut found = Vec::new();
        declared_namespaces(node, recursive, modern, &mut found);
        namespace_array(req, found).map(OrFalse::Value)
    }

    /// `children(?string $namespaceOrPrefix = null, bool $isPrefix = false): ?SimpleXMLElement`:
    /// the node's child elements, in a namespace where one is named.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r xmlns:p="urn:p"><a/><p:b/><c/></r>');
    /// foreach ($s->children() as $n => $c) echo $n, ' ';
    /// foreach ($s->children('p', true) as $n => $c) echo $n, ' ';
    /// echo count($s->children('urn:p')), "\n";
    /// var_dump($s->attributes()->children());
    /// ```
    /// ```expect
    /// a c b 1
    /// NULL
    /// ```
    #[php(tentative)]
    fn children(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] namespaceOrPrefix: Option<PhpString>,
        #[php(default = false)] isPrefix: bool,
    ) -> Result<Option<Owner<Instance<SimpleXMLElement>>>, Thrown> {
        let this = this.object();
        let view = View::of(req, this);
        if matches!(view.walk, Walk::Attributes(_)) {
            return Ok(None);
        }
        let base = view.node(req)?;
        let Some(node) = view.first(base) else {
            return Ok(None);
        };
        let ns = Ns::of(
            namespaceOrPrefix.as_ref().map(PhpString::as_bytes),
            isPrefix,
        );
        wrap(req, this, node, Walk::Children, ns).map(|o| Some(o.into_instance()))
    }

    /// `attributes(?string $namespaceOrPrefix = null, bool $isPrefix = false): ?SimpleXMLElement`:
    /// the node's attributes, in a namespace where one is named.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r xmlns:p="urn:p" a="1" p:b="2"/>');
    /// foreach ([$s->attributes(), $s->attributes('urn:p')] as $list) {
    ///     foreach ($list as $n => $v) echo $n, '=', $v, ';';
    /// }
    /// echo "\n";
    /// var_dump($s->attributes()->attributes());
    /// ```
    /// ```expect
    /// a=1;b=2;
    /// NULL
    /// ```
    #[php(tentative)]
    fn attributes(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] namespaceOrPrefix: Option<PhpString>,
        #[php(default = false)] isPrefix: bool,
    ) -> Result<Option<Owner<Instance<SimpleXMLElement>>>, Thrown> {
        let this = this.object();
        let view = View::of(req, this);
        let base = view.node(req)?;
        let Some(node) = view.first(base) else {
            return Ok(None);
        };
        if matches!(view.walk, Walk::Attributes(_)) {
            return Ok(None);
        }
        let ns = Ns::of(
            namespaceOrPrefix.as_ref().map(PhpString::as_bytes),
            isPrefix,
        );
        wrap(req, this, node, Walk::Attributes(None), ns).map(|o| Some(o.into_instance()))
    }

    /// `__construct(string $data, int $options = 0, bool $dataIsURL = false, string $namespaceOrPrefix = "", bool $isPrefix = false)`:
    /// the document parsed from the string or the file, the object
    /// standing for its root; php's `Exception` where it is not XML.
    ///
    /// ```php
    /// $s = new SimpleXMLElement('<r><a>1</a></r>');
    /// echo $s->a, "\n";
    /// try { @new SimpleXMLElement('<r'); } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// 1
    /// Exception: String could not be parsed as XML
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        data: PhpString,
        #[php(default = 0)] options: i64,
        #[php(default = false)] dataIsURL: bool,
        #[php(default = "")] namespaceOrPrefix: PhpString,
        #[php(default = false)] isPrefix: bool,
    ) -> Result<(), Thrown> {
        let this = this.object();
        if exceeds_int(options) {
            return Err(argument_error(req, "Exception", 2, "is invalid"));
        }
        let source = if dataIsURL {
            Source::File(data.as_bytes())
        } else {
            Source::Memory(data.as_bytes())
        };
        let Some(doc) = parse(req, source, options)? else {
            return Err(throw(req, "Exception", "String could not be parsed as XML"));
        };
        if let Some(s) = sxe_mut(req, this) {
            s.xpath = None;
        }
        if let Some(mut obj) = sxe::libxml_half(req, this) {
            obj.decrement_resource(req);
        }
        let released = sxe_mut(req, this).and_then(|s| s.current.take());
        if let Some(old) = released {
            old.give_up(req);
        }
        let ns = Ns::of(Some(namespaceOrPrefix.as_bytes()), isPrefix);
        adopt(req, this, doc, ns);
        Ok(())
    }

    /// `addChild(string $qualifiedName, ?string $value = null, ?string $namespace = null): ?SimpleXMLElement`:
    /// a child element appended, `$value` its content as markup, in the
    /// namespace named, declared on it where no ancestor declares it.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r xmlns:p="urn:p"/>');
    /// $s->addChild('a', 'x &amp; y');
    /// $s->addChild('p:b', null, 'urn:p');
    /// $s->addChild('q:c', 'z', 'urn:q');
    /// echo $s->asXML();
    /// var_dump($s->attributes()->addChild('d'));
    /// try { $s->addChild(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// <?xml version="1.0"?>
    /// <r xmlns:p="urn:p"><a>x &amp; y</a><p:b/><q:c xmlns:q="urn:q">z</q:c></r>
    ///
    /// Warning: SimpleXMLElement::addChild(): Cannot add element to attributes in %s on line 7
    /// NULL
    /// SimpleXMLElement::addChild(): Argument #1 ($qualifiedName) must not be empty
    /// ```
    #[php(tentative)]
    fn addChild(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: PhpString,
        #[php(default = null)] value: Option<PhpString>,
        #[php(default = null)] namespace: Option<PhpString>,
    ) -> Result<Option<Owner<Instance<SimpleXMLElement>>>, Thrown> {
        let this = this.object();
        if qualifiedName.is_empty() {
            return Err(arg_value_error(req, 1, "not be empty"));
        }
        let view = View::of(req, this);
        let base = view.node(req)?;
        if matches!(view.walk, Walk::Attributes(_)) {
            docref(req, Level::Warning, "Cannot add element to attributes");
            return Ok(None);
        }
        let Some(node) = view.first(base) else {
            docref(
                req,
                Level::Warning,
                "Cannot add child. Parent is not a permanent member of the XML tree",
            );
            return Ok(None);
        };
        node.invalidate_document_lists();
        let (prefix, local) = split(qualifiedName);
        let content = value.as_ref().map(PhpString::as_bytes);
        let Some(made) = node.new_child(None, &local, content) else {
            return Ok(None);
        };
        if let Some(uri) = namespace {
            let uri = c_name(uri.as_bytes());
            if uri.is_empty() {
                made.set_ns(None);
                made.new_ns(&uri, prefix.as_deref());
            } else {
                let ns = node
                    .search_ns_by_href(node.doc(), &uri)
                    .or_else(|| made.new_ns(&uri, prefix.as_deref()));
                made.set_ns(ns);
            }
        }
        let ns = Ns::of(prefix.as_deref(), false);
        wrap(req, this, made, Walk::Node, ns).map(|o| Some(o.into_instance()))
    }

    /// `addAttribute(string $qualifiedName, string $value, ?string $namespace = null): void`:
    /// an attribute added where the element has none of the name, a
    /// namespaced one needing a prefix.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r/>');
    /// $s->addAttribute('a', '1');
    /// $s->addAttribute('p:b', '2', 'urn:p');
    /// $s->addAttribute('a', '3');
    /// $s->addAttribute('c', '4', 'urn:q');
    /// echo $s->asXML();
    /// ```
    /// ```expect
    /// Warning: SimpleXMLElement::addAttribute(): Attribute already exists in %s on line 5
    ///
    /// Warning: SimpleXMLElement::addAttribute(): Attribute requires prefix for namespace in %s on line 6
    /// <?xml version="1.0"?>
    /// <r xmlns:p="urn:p" a="1" p:b="2"/>
    /// ```
    #[php(tentative)]
    fn addAttribute(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: PhpString,
        value: PhpString,
        #[php(default = null)] namespace: Option<PhpString>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        if qualifiedName.is_empty() {
            return Err(arg_value_error(req, 1, "not be empty"));
        }
        let view = View::of(req, this);
        let base = view.node(req)?;
        let node = view.first(base).and_then(|n| {
            if n.kind() == Kind::Element {
                Some(n)
            } else {
                n.parent()
            }
        });
        let Some(node) = node else {
            docref(req, Level::Warning, "Unable to locate parent Element");
            return Ok(());
        };
        let uri = namespace.map(|n| c_name(n.as_bytes()));
        let (prefix, local) = split(qualifiedName);
        if prefix.is_none() && uri.as_ref().is_some_and(|u| !u.is_empty()) {
            docref(
                req,
                Level::Warning,
                "Attribute requires prefix for namespace",
            );
            return Ok(());
        }
        if node
            .has_ns_prop(&local, uri.as_deref())
            .is_some_and(|a| a.kind() != Kind::AttributeDecl)
        {
            docref(req, Level::Warning, "Attribute already exists");
            return Ok(());
        }
        let ns = uri.as_ref().and_then(|u| {
            node.search_ns_by_href(node.doc(), u)
                .or_else(|| node.new_ns(u, prefix.as_deref()))
        });
        node.new_ns_prop(ns, &local, Some(value.as_bytes()));
        Ok(())
    }

    /// `getName(): string`: the name of the walk's first node.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r><a/></r>');
    /// var_dump($s->getName(), $s->a->getName(), $s->b->getName());
    /// ```
    /// ```expect
    /// string(1) "r"
    /// string(1) "a"
    /// string(0) ""
    /// ```
    #[php(tentative)]
    fn getName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let view = View::of(req, this.object());
        let base = view.node(req)?;
        let name = view.first(base).map(Node::name).unwrap_or_default();
        // `zend_string_finit_fast` (`Z/ext/simplexml/simplexml.c:1677`).
        PhpString::fast(req, &name)
    }

    /// `__toString(): string`: the text of the walk's first node.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r>a<b>x</b>c<!--n--></r>');
    /// var_dump($s->__toString(), (string) $s->b, "{$s->missing}");
    /// ```
    /// ```expect
    /// string(2) "ac"
    /// string(1) "x"
    /// string(0) ""
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let text = sxe::cast_text(req, this.object())?.unwrap_or_default();
        PhpString::new(req, &text)
    }

    /// `__debugInfo(): ?array`: what `var_dump` shows, the attributes under
    /// `@attributes` and the children by name.
    ///
    /// ```php
    /// var_dump(simplexml_load_string('<r a="1"><b>x</b><b>y</b><c><d/></c></r>'));
    /// ```
    /// ```expect
    /// object(SimpleXMLElement)#1 (3) {
    ///   ["@attributes"]=>
    ///   array(1) {
    ///     ["a"]=>
    ///     string(1) "1"
    ///   }
    ///   ["b"]=>
    ///   array(2) {
    ///     [0]=>
    ///     string(1) "x"
    ///     [1]=>
    ///     string(1) "y"
    ///   }
    ///   ["c"]=>
    ///   object(SimpleXMLElement)#2 (1) {
    ///     ["d"]=>
    ///     object(SimpleXMLElement)#3 (0) {
    ///     }
    ///   }
    /// }
    /// ```
    fn __debugInfo(req: &mut Request, this: This<Self>) -> Result<Option<Owner<PhpArray>>, Thrown> {
        sxe::debug_table(req, this.object()).map(Some)
    }

    /// `count(): int`: the elements the walk takes.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r><a/><a/><b/></r>');
    /// var_dump($s->count(), $s->a->count(), count($s->children()));
    /// ```
    /// ```expect
    /// int(3)
    /// int(2)
    /// int(3)
    /// ```
    #[php(tentative)]
    fn count(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        sxe::count(req, this.object())
    }

    /// `rewind(): void`: the object's own walk set at its first element.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r><a>1</a><b>2</b></r>');
    /// for ($s->rewind(); $s->valid(); $s->next()) echo $s->key(), '=', $s->current(), ';';
    /// echo "\n";
    /// try { $s->current(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// a=1;b=2;
    /// Iterator not initialized or already consumed
    /// ```
    #[php(tentative)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        sxe::rewind(req, this.object())
    }

    /// `valid(): bool`.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r><a/></r>');
    /// var_dump($s->valid()); $s->rewind(); var_dump($s->valid());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn valid(req: &mut Request, this: This<Self>) -> bool {
        sxe(req, this.object()).is_some_and(|s| s.current.is_some())
    }

    /// `current(): SimpleXMLElement`.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r><a>1</a></r>');
    /// $s->rewind(); var_dump((string) $s->current());
    /// ```
    /// ```expect
    /// string(1) "1"
    /// ```
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<Element, Thrown> {
        match sxe::current(req, this.object()) {
            Some(value) => Element::from_value(req, value),
            None => Err(throw_error(
                req,
                "Error",
                "Iterator not initialized or already consumed",
            )),
        }
    }

    /// `key(): string`: the name of the current element.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r><a>1</a></r>');
    /// try { $s->key(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $s->rewind(); var_dump($s->key());
    /// ```
    /// ```expect
    /// Iterator not initialized or already consumed
    /// string(1) "a"
    /// ```
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        match sxe::current_name(req, this.object()) {
            Some(name) => PhpString::new(req, &name),
            None => Err(throw_error(
                req,
                "Error",
                "Iterator not initialized or already consumed",
            )),
        }
    }

    /// `next(): void`.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r><a/><b/></r>');
    /// $s->rewind(); $s->next(); var_dump($s->key()); $s->next(); var_dump($s->valid());
    /// ```
    /// ```expect
    /// string(1) "b"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        sxe::move_forward(req, this.object())
    }

    /// `hasChildren(): bool`: whether the current element has an element
    /// child.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r><a><b/></a><c>t</c></r>');
    /// for ($s->rewind(); $s->valid(); $s->next()) var_dump($s->hasChildren());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn hasChildren(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = this.object();
        let attributes = matches!(View::of(req, this).walk, Walk::Attributes(_));
        let Some(current) = sxe(req, this).and_then(|s| s.current.as_deref().copied()) else {
            return Ok(false);
        };
        if attributes {
            return Ok(false);
        }
        let node = View::of(req, current).node(req)?;
        let mut at = node.children();
        while let Some(n) = at {
            if n.kind() == Kind::Element {
                return Ok(true);
            }
            at = n.next();
        }
        Ok(false)
    }

    /// `getChildren(): ?SimpleXMLElement`: the current element.
    ///
    /// ```php
    /// $s = simplexml_load_string('<r><a><b/></a></r>');
    /// var_dump($s->getChildren());
    /// $s->rewind(); var_dump($s->getChildren()->getName());
    /// ```
    /// ```expect
    /// NULL
    /// string(1) "a"
    /// ```
    #[php(tentative)]
    fn getChildren(req: &mut Request, this: This<Self>) -> Result<Option<Element>, Thrown> {
        let this = this.object();
        if matches!(View::of(req, this).walk, Walk::Attributes(_)) {
            return Ok(None);
        }
        match sxe::current(req, this) {
            Some(value) => Element::from_value(req, value).map(Some),
            None => Ok(None),
        }
    }
}

/// `class SimpleXMLIterator extends SimpleXMLElement`.
///
/// ```php
/// $i = new SimpleXMLIterator('<r><a><b>1</b></a><c>2</c></r>');
/// foreach (new RecursiveIteratorIterator($i) as $k => $v) echo $k, '=', $v, ';';
/// echo "\n";
/// ```
/// ```expect
/// b=1;c=2;
/// ```
#[php_class(extends(SimpleXMLElement))]
pub(crate) struct SimpleXMLIterator;

#[php_class]
impl SimpleXMLIterator {}

/// The object's evaluator, made the first time (`xmlXPathNewContext` over
/// its document), taken off the object while a call uses it.
fn evaluator(req: &mut Request, this: PhpObject) -> Option<XPath> {
    let doc = sxe(req, this).and_then(|s| s.obj.document_node());
    let s = sxe_mut(req, this)?;
    s.xpath.take().or_else(|| doc.and_then(XPath::bare))
}

/// `xmlSplitQName2`: the prefix and the local part of a qualified name,
/// the name itself where it has no prefix.
fn split(qname: PhpString) -> (Option<Vec<u8>>, Vec<u8>) {
    let qname = c_name(qname.as_bytes());
    match Name::split2(&qname) {
        Some((local, prefix)) => (Some(prefix), local),
        None => (None, qname),
    }
}

/// `asXML`: the document's handlers' `dump_doc_to_str` where the node is
/// its root, `dump_node_to_str` otherwise, or the file variants.
fn as_xml(
    req: &mut Request,
    this: PhpObject,
    filename: Option<Path>,
) -> Result<Union<Owner<PhpString>, bool>, Thrown> {
    let view = View::of(req, this);
    let base = view.node(req)?;
    let Some(node) = view.first(base) else {
        return Ok(Union::B(false));
    };
    let Some((doc, handlers)) = sxe(req, this).and_then(|s| {
        let doc = s.obj.document_node()?;
        Some((doc, s.obj.document_handlers()))
    }) else {
        return Ok(Union::B(false));
    };
    let whole = node.parent().is_some_and(|p| p.kind() == Kind::Document);
    let encoding = doc.doc_encoding();
    let encoding = encoding.as_deref();
    if let Some(path) = filename {
        let path = path.0.as_bytes();
        let written = if whole {
            handlers.dump_doc_to_file(req, path, doc, false, false, encoding)?
        } else {
            handlers.dump_node_to_file(req, path, doc, node, false)?
        };
        return Ok(Union::B(written != -1));
    }
    let text = if whole {
        handlers.dump_doc_to_str(req, doc, 0, false, encoding)?
    } else {
        handlers.dump_node_to_str(req, doc, node, false, false, encoding)?
    };
    match text {
        Some(t) => PhpString::new(req, &t).map(Union::A),
        None => Ok(Union::B(false)),
    }
}

/// `sxe_add_namespace_name_raw`: `prefix` (`""` for none) to `href`,
/// where the prefix is not there yet.
fn add_namespace(
    found: &mut Vec<(Vec<u8>, Vec<u8>)>,
    prefix: Option<Vec<u8>>,
    href: Option<Vec<u8>>,
) {
    let prefix = prefix.unwrap_or_default();
    if !found.iter().any(|(p, _)| *p == prefix) {
        found.push((prefix, href.unwrap_or_default()));
    }
}

/// `sxe_add_namespaces`.
fn used_namespaces(node: Node, recursive: bool, found: &mut Vec<(Vec<u8>, Vec<u8>)>) {
    if let Some(ns) = node.ns() {
        add_namespace(found, ns.prefix(), ns.href());
    }
    let mut attr = node.properties();
    while let Some(a) = attr {
        if let Some(ns) = a.ns() {
            add_namespace(found, ns.prefix(), ns.href());
        }
        attr = a.next();
    }
    if recursive {
        let mut child = node.children();
        while let Some(c) = child {
            if c.kind() == Kind::Element {
                used_namespaces(c, recursive, found);
            }
            child = c.next();
        }
    }
}

/// `sxe_add_registered_namespaces`.
fn declared_namespaces(
    node: Node,
    recursive: bool,
    modern: bool,
    found: &mut Vec<(Vec<u8>, Vec<u8>)>,
) {
    if node.kind() != Kind::Element {
        return;
    }
    let mut ns = node.ns_def();
    while let Some(n) = ns {
        add_namespace(found, n.prefix(), n.href());
        ns = n.next();
    }
    if modern {
        let mut attr = node.properties();
        while let Some(a) = attr {
            if let Some(ns) = a.ns()
                && ns.href().as_deref() == Some(b"http://www.w3.org/2000/xmlns/".as_slice())
            {
                let prefix = if ns.prefix().is_some() {
                    a.name()
                } else {
                    Vec::new()
                };
                add_namespace(found, Some(prefix), Some(a.attr_value()));
            }
            attr = a.next();
        }
    }
    if recursive {
        let mut child = node.children();
        while let Some(c) = child {
            declared_namespaces(c, recursive, modern, found);
            child = c.next();
        }
    }
}

/// The namespaces found as php's array, prefix to URI.
fn namespace_array(
    req: &mut Request,
    found: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for (prefix, href) in found {
        let value = sxe::string(req, &href)?;
        out.set_str(req, &prefix, value)?;
    }
    Ok(out)
}
