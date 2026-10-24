//! `DOMNodeList` and `DOMNamedNodeMap` (`obj_map.c`, `nodelist.c`,
//! `namednodemap.c`, `dom_iterators.c`): a live view over a node's
//! children, its attributes, the elements under it of one name, a DTD's
//! entities or notations, or the node set an `XPath` query answered.

use php_ext::abi::{
    ClassIterator, DimensionRead, GcTable, Instance, ObjectHandlers, PropertyPlace, PropertyRead,
    PropertyTest, Raw, Refusals, This, Union, Untyped,
};
use php_ext::attr::php_class;
use php_ext::coerce::{numeric_string, to_long};
use php_ext::raising::{Thrown, throw_error};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};
use php_ext_libxml::ffi::{DtdTable, Kind, Node};

use crate::object::{Dom, Extra};
use crate::{class_name, dom, dom_mut, node_of, object_for, string};

/// What a map lists: `dom_nnodemap_object`'s handler and the fields it
/// reads.
pub(crate) enum MapKind {
    /// `php_dom_obj_map_child_nodes`.
    ChildNodes,
    /// `php_dom_obj_map_attributes`.
    Attributes,
    /// `php_dom_obj_map_by_tag_name`: `ns` none for any namespace, empty
    /// for none; `qname` where the name is matched as a qualified name,
    /// lowercased for an HTML element of an HTML document, the modern
    /// classes' match when no namespace is given.
    ByTagName {
        local: Vec<u8>,
        ns: Option<Vec<u8>>,
        qname: bool,
    },
    /// `php_dom_obj_map_by_class_name`: the tokens every match carries.
    ByClassName(Vec<Vec<u8>>),
    /// `php_dom_obj_map_child_elements`.
    ChildElements,
    /// `php_dom_obj_map_noop`: nothing, what `getElementsByClassName`
    /// answers for no token.
    Noop,
    /// `php_dom_obj_map_entities`.
    Entities,
    /// `php_dom_obj_map_notations`.
    Notations,
    /// `php_dom_obj_map_nodeset`: the objects, the map's owner of them.
    NodeSet(Owner<PhpArray>),
}

/// `dom_nnodemap_object`.
pub(crate) struct Map {
    /// `baseobj`, the map's owner of it; none for a node set.
    pub(crate) base: Option<Owner<PhpObject>>,
    pub(crate) kind: MapKind,
    /// The `read_dimension` and `has_dimension` its class installs.
    pub(crate) dims: Dims,
}

/// The dimension handlers of the five list and map classes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dims {
    /// `dom_nodelist_read_dimension` and `dom_nodemap_read_dimension`,
    /// told apart by whether the map is named ([`named_keys`]).
    Legacy,
    /// `dom_modern_nodelist_read_dimension`: an index only.
    ModernList,
    /// `dom_modern_nodemap_read_dimension`: an index, or a name.
    ModernMap,
    /// `dom_html_collection_read_dimension`: an index, or `namedItem`.
    Collection,
}

impl Dims {
    /// The handlers of the class named.
    pub(crate) fn of(class: &str) -> Dims {
        match class {
            "Dom\\NodeList" => Dims::ModernList,
            "Dom\\NamedNodeMap" | "Dom\\DtdNamedNodeMap" => Dims::ModernMap,
            "Dom\\HTMLCollection" => Dims::Collection,
            _ => Dims::Legacy,
        }
    }
}

/// The view `object` is, what a reader needs of it copied out.
enum View {
    Nodes {
        base: PhpObject,
        node: Option<Node>,
        kind: Walk,
    },
    Set(PhpArray),
    None,
}

/// A walk over nodes.
#[derive(Clone)]
enum Walk {
    Children,
    ChildElements,
    Attributes,
    ByTagName {
        local: Vec<u8>,
        ns: Option<Vec<u8>>,
        qname: bool,
    },
    ByClassName {
        tokens: Vec<Vec<u8>>,
        quirks: bool,
    },
    Table(DtdTable),
    Noop,
}

fn view(req: &Request, object: PhpObject) -> View {
    let Some(Dom {
        extra: Extra::Map(map),
        ..
    }) = dom(req, object)
    else {
        return View::None;
    };
    let Some(base) = map.base.as_deref().copied() else {
        if let MapKind::NodeSet(array) = &map.kind {
            return View::Set(**array);
        }
        return View::None;
    };
    let walk = match &map.kind {
        MapKind::NodeSet(array) => return View::Set(**array),
        MapKind::ChildNodes => Walk::Children,
        MapKind::ChildElements => Walk::ChildElements,
        MapKind::Attributes => Walk::Attributes,
        MapKind::ByTagName { local, ns, qname } => Walk::ByTagName {
            local: local.clone(),
            ns: ns.clone(),
            qname: *qname,
        },
        MapKind::ByClassName(tokens) => Walk::ByClassName {
            tokens: tokens.clone(),
            quirks: dom(req, base)
                .is_some_and(|d| d.obj.quirks() == php_ext_libxml::ffi::Quirks::Quirks),
        },
        MapKind::Entities => Walk::Table(DtdTable::Entities),
        MapKind::Notations => Walk::Table(DtdTable::Notations),
        MapKind::Noop => Walk::Noop,
    };
    View::Nodes {
        base,
        node: node_of(req, base),
        kind: walk,
    }
}

/// `php_dom_first_child_of_container_node`.
fn first_of_container(node: Node) -> Option<Node> {
    if node.kind().is_document() {
        node.root()
    } else {
        node.children()
    }
}

/// `dom_match_qualified_name_according_to_spec`: `qname` against the
/// node's prefix and local name.
///
/// ```php
/// $h = Dom\HTMLDocument::createFromString('<!doctype html><p DATA-X=1>x</p>');
/// $p = $h->body->firstChild;
/// var_dump($p->getAttributeNode('Data-X')->name, $p->attributes['DATA-x']->value, $h->getElementsByTagName('P')->length);
/// $x = Dom\XMLDocument::createFromString('<r xmlns:q="urn:q"><q:a q:b="1"/><A/></r>');
/// var_dump($x->getElementsByTagName('q:a')->length, $x->getElementsByTagName('a')->length, $x->documentElement->firstChild->getAttributeNode('q:b')->value);
/// ```
/// ```expect
/// string(6) "data-x"
/// string(1) "1"
/// int(1)
/// int(1)
/// int(0)
/// string(1) "1"
/// ```
pub(crate) fn qname_matches(qname: &[u8], node: Node) -> bool {
    match node.ns().and_then(php_ext_libxml::ffi::Ns::prefix) {
        Some(prefix) => {
            qname.len() > prefix.len()
                && qname.starts_with(&prefix)
                && qname.get(prefix.len()) == Some(&b':')
                && qname.get(prefix.len() + 1..) == Some(node.name().as_slice())
        }
        None => node.name() == qname,
    }
}

/// `dom_get_elements_by_tag_name_ns_raw`'s test of one element.
fn tag_matches(node: Node, local: &[u8], ns: Option<&[u8]>, qname: bool) -> bool {
    if node.kind() != Kind::Element {
        return false;
    }
    let local_ok = local == b"*"
        || if qname {
            if crate::private::html_in_html_document(node) {
                qname_matches(&local.to_ascii_lowercase(), node)
            } else {
                qname_matches(local, node)
            }
        } else {
            node.name() == local
        };
    let ns_ok = match ns {
        None | Some(b"*") => true,
        Some(b"") => node.ns().is_none(),
        Some(uri) => node.ns().and_then(php_ext_libxml::ffi::Ns::href).as_deref() == Some(uri),
    };
    local_ok && ns_ok
}

/// The nodes a walk lists, in order.
fn nodes(node: Node, walk: &Walk) -> Vec<Node> {
    let mut out = Vec::new();
    match walk {
        Walk::Children => {
            if node.kind() == Kind::EntityRef {
                crate::sync_entity_reference(node);
            }
            let mut at = node.children();
            while let Some(n) = at {
                out.push(n);
                at = n.next();
            }
        }
        Walk::Attributes => {
            let mut at = node.properties();
            while let Some(n) = at {
                out.push(n);
                at = n.next();
            }
        }
        Walk::ChildElements => {
            let mut at = node.children();
            while let Some(n) = at {
                if n.kind() == Kind::Element {
                    out.push(n);
                }
                at = n.next();
            }
        }
        Walk::ByTagName { local, ns, qname } => {
            let mut at = first_of_container(node);
            while let Some(n) = at {
                if tag_matches(n, local, ns.as_deref(), *qname) {
                    out.push(n);
                }
                at = crate::next_in_tree_order(n, node);
            }
        }
        Walk::ByClassName { tokens, quirks } => {
            let mut at = first_of_container(node);
            while let Some(n) = at {
                if matches_class_name(n, tokens, *quirks) {
                    out.push(n);
                }
                at = crate::next_in_tree_order(n, node);
            }
        }
        Walk::Table(_) | Walk::Noop => {}
    }
    out
}

/// `dom_matches_class_name`.
fn matches_class_name(node: Node, tokens: &[Vec<u8>], quirks: bool) -> bool {
    if node.kind() != Kind::Element {
        return false;
    }
    let Some(attr) = node.has_ns_prop(b"class", None) else {
        return false;
    };
    crate::token_list::all_contained(tokens, &attr.attr_value(), quirks)
}

/// `php_dom_get_nodelist_length` and `php_dom_get_namednodemap_length`.
fn length(req: &mut Request, object: PhpObject) -> i64 {
    match view(req, object) {
        View::Set(array) => i64::try_from(array.len()).unwrap_or(i64::MAX),
        View::Nodes {
            node: Some(node),
            kind: Walk::Table(which),
            ..
        } => i64::try_from(
            node.dtd_table(which)
                .map_or(0, php_ext_libxml::ffi::HashTable::len),
        )
        .unwrap_or(0),
        View::Nodes {
            node: Some(node),
            kind,
            ..
        } => i64::try_from(nodes(node, &kind).len()).unwrap_or(0),
        _ => 0,
    }
}

/// `handler->get_item`: the object at `index`, `null` past the end.
fn item(req: &mut Request, object: PhpObject, index: i64) -> Result<Owner<Value>, Thrown> {
    let Ok(at) = usize::try_from(index) else {
        return Ok(Owner::null());
    };
    match view(req, object) {
        View::Set(array) => match array.get(&Key::Int(index)) {
            Some(v) => Ok(Owner::take(req, v)),
            None => Ok(Owner::null()),
        },
        View::Nodes {
            base,
            node: Some(node),
            kind: Walk::Table(which),
        } => {
            let found = node.dtd_table(which).and_then(|t| match which {
                DtdTable::Entities => t.entity_at(at),
                DtdTable::Notations => t.notation_at(at),
            });
            match found {
                Some(n) => object_for(req, n, Some(base)),
                None => Ok(Owner::null()),
            }
        }
        View::Nodes {
            base,
            node: Some(node),
            kind,
        } => match nodes(node, &kind).get(at) {
            Some(n) => object_for(req, *n, Some(base)),
            None => Ok(Owner::null()),
        },
        _ => Ok(Owner::null()),
    }
}

/// `handler->get_ns_named_item`.
fn named(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
    ns: Option<&[u8]>,
) -> Result<Owner<Value>, Thrown> {
    let View::Nodes {
        base,
        node: Some(node),
        kind,
    } = view(req, object)
    else {
        return Ok(Owner::null());
    };
    let modern = crate::modern::follows_spec(req, base);
    // An empty namespace is none to a document that follows the standard
    // (php 8.5.12), and a DTD's default is no attribute of the element.
    let ns = ns.filter(|uri| !(uri.is_empty() && modern));
    let found = match kind {
        Walk::Attributes => match ns {
            Some(uri) => crate::carried(node.has_ns_prop(name, Some(uri))),
            None if modern => attribute_node(node, name),
            None => node.has_prop(name),
        },
        Walk::Table(DtdTable::Entities) => node
            .dtd_table(DtdTable::Entities)
            .and_then(|t| t.entity(name)),
        Walk::Table(DtdTable::Notations) => node
            .dtd_table(DtdTable::Notations)
            .and_then(|t| t.notation(name)),
        _ => None,
    };
    match found {
        Some(n) => object_for(req, n, Some(base)),
        None => Ok(Owner::null()),
    }
}

/// `php_dom_get_attribute_node`: the first attribute whose qualified name
/// is `name`, lowercased first on an HTML element of an HTML document.
///
/// ```php
/// $h = Dom\HTMLDocument::createFromString('<!doctype html><p DATA-X=1>x</p>');
/// $p = $h->body->firstChild;
/// var_dump($p->getAttributeNode('Data-X')->name, $p->attributes['DATA-x']->value, $h->getElementsByTagName('P')->length);
/// $x = Dom\XMLDocument::createFromString('<r xmlns:q="urn:q"><q:a q:b="1"/><A/></r>');
/// var_dump($x->getElementsByTagName('q:a')->length, $x->getElementsByTagName('a')->length, $x->documentElement->firstChild->getAttributeNode('q:b')->value);
/// ```
/// ```expect
/// string(6) "data-x"
/// string(1) "1"
/// int(1)
/// int(1)
/// int(0)
/// string(1) "1"
/// ```
pub(crate) fn attribute_node(elem: Node, name: &[u8]) -> Option<Node> {
    let name = if crate::private::html_in_html_document(elem) {
        name.to_ascii_lowercase()
    } else {
        name.to_vec()
    };
    let mut at = elem.properties();
    while let Some(attr) = at {
        if qname_matches(&name, attr) {
            return Some(attr);
        }
        at = attr.next();
    }
    None
}

/// `dom_html_collection_named_item`: the first element of the collection
/// whose `id` is `key`, or, in the HTML namespace, whose `name` is.
fn collection_named(
    req: &mut Request,
    object: PhpObject,
    key: &[u8],
) -> Result<Owner<Value>, Thrown> {
    if key.is_empty() {
        return Ok(Owner::null());
    }
    let View::Nodes {
        base,
        node: Some(node),
        kind,
    } = view(req, object)
    else {
        return Ok(Owner::null());
    };
    for candidate in nodes(node, &kind) {
        let id = candidate.has_ns_prop(b"id", None);
        let hit = id.is_some_and(|a| a.attr_value() == key)
            || (crate::private::ns_is(candidate, crate::private::HTML_NS)
                && candidate
                    .has_ns_prop(b"name", None)
                    .is_some_and(|a| a.attr_value() == key));
        if hit {
            return object_for(req, candidate, Some(base));
        }
    }
    Ok(Owner::null())
}

/// `dom_modern_nodelist_get_index`: an index, a name, or neither.
enum Index {
    Long(i64),
    Str(PhpString),
    Illegal,
}

fn modern_index(req: &mut Request, value: &Value) -> Result<Index, Thrown> {
    let v = value.deref();
    Ok(match v.as_ref() {
        ValueRef::Int(n) => Index::Long(n),
        ValueRef::Float(f) => Index::Long(float_offset(req, f)?),
        ValueRef::Str(s) => match Key::lookup(s) {
            Key::Int(n) => Index::Long(n),
            Key::Str(s) => Index::Str(s),
        },
        _ => Index::Illegal,
    })
}

/// `zend_dval_to_lval_safe`: the integer a float offset makes, with php's
/// deprecation where the cast drops a fraction.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r><a/><b/></r>');
/// $l = $d->documentElement->childNodes;
/// var_dump($l[1.0]->localName, $l[1.1]->localName, isset($d->documentElement->classList[0.5]));
/// ```
/// ```expect
///
/// Deprecated: Implicit conversion from float 1.1 to int loses precision in %s on line 4
///
/// Deprecated: Implicit conversion from float 0.5 to int loses precision in %s on line 4
/// string(1) "b"
/// string(1) "b"
/// bool(false)
/// ```
pub(crate) fn float_offset(req: &mut Request, f: f64) -> Result<i64, Thrown> {
    let n = php_ext::coerce::float_to_long(f);
    // `zend_is_long_compatible` is `(double)l == d`, an exact test.
    #[allow(clippy::cast_precision_loss, clippy::float_cmp)]
    let compatible = (n as f64) == f;
    if !compatible && f.is_finite() && f.abs() < 9.2e18 {
        let shown = php_ext::coerce::with_string(req, &Value::float(f), |text| {
            String::from_utf8_lossy(text).into_owned()
        })?;
        php_ext::raising::deprecated(
            req,
            &format!("Implicit conversion from float {shown} to int loses precision"),
        );
    }
    Ok(n)
}

/// `zend_illegal_container_offset(object->ce->name, offset, read)`.
fn illegal_offset(
    req: &mut Request,
    object: PhpObject,
    value: &Value,
    read: DimensionRead,
) -> Thrown {
    let class = req.symbols().class_name(object.class());
    php_ext::raising::illegal_container_offset(req, &class, value, read)
}

/// The dimensions a map's class reads, `Dims`.
fn dims(req: &Request, object: PhpObject) -> Dims {
    match dom(req, object) {
        Some(Dom {
            extra: Extra::Map(m),
            ..
        }) => m.dims,
        _ => Dims::Legacy,
    }
}

/// Whether the map is keyed by name as it is iterated: attributes and a
/// DTD's tables, `handler->nameless` false.
fn named_keys(req: &Request, object: PhpObject) -> bool {
    matches!(
        dom(req, object),
        Some(Dom { extra: Extra::Map(m), .. })
            if matches!(m.kind, MapKind::Attributes | MapKind::Entities | MapKind::Notations)
    )
}

/// `dom_nnodemap_or_nodelist_process_offset_as_named`: `Err` with the
/// name for a string that is no number, the index otherwise.
fn offset(req: &mut Request, value: &Value) -> Result<Result<i64, PhpString>, Thrown> {
    let v = value.deref();
    if let ValueRef::Str(s) = v.as_ref() {
        return Ok(match numeric_string(s.as_bytes()) {
            None => Err(s),
            Some(n) => Ok(to_long(req, &n)?),
        });
    }
    Ok(Ok(to_long(req, &v)?))
}

/// `dom_nodelist_length_read` and `dom_namednodemap_length_read`.
#[allow(clippy::unnecessary_wraps)] // the reader shape every row of a property table has
fn length_read(req: &mut Request, object: PhpObject) -> crate::Read {
    let length = length(req, object);
    Ok(Some(Owner::int(length)))
}

/// The `length` property both classes have.
pub(crate) const PROPS: &[crate::props::Prop] = &[crate::props::Prop {
    name: b"length",
    read: length_read,
    write: None,
}];

/// `dom_nodelist_object_handlers` and `dom_nnodemap_object_handlers`.
pub(crate) struct MapHandlers;

pub(crate) static HANDLERS: MapHandlers = MapHandlers;

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for MapHandlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `php_dom_iterator`: the iterator, `curobj`, `index` and the cache tag.
    fn iterator_size(&self) -> usize {
        120
    }

    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        crate::read_property(req, object, name, read)
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        crate::write_property(req, object, name, value)
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        crate::property_place(req, object, name)
    }

    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        crate::has_property(req, object, name, test)
    }

    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        crate::unset_property(req, object, name)
    }

    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        crate::debug_info(req, object)
    }

    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset_value: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        let dims = dims(req, object);
        let Some(offset_value) = offset_value else {
            let name = class_name(req, object.class());
            let message = if dims == Dims::Legacy {
                format!("Cannot access {name} without offset")
            } else {
                format!("Cannot append to {name}")
            };
            return Err(throw_error(req, "Error", &message));
        };
        if dims != Dims::Legacy {
            return match (dims, modern_index(req, offset_value)?) {
                (_, Index::Long(n)) => item(req, object, n),
                (Dims::ModernMap, Index::Str(name)) => named(req, object, name.as_bytes(), None),
                (Dims::Collection, Index::Str(name)) => {
                    collection_named(req, object, name.as_bytes())
                }
                _ => Err(illegal_offset(req, object, offset_value, read)),
            };
        }
        let is_list = !named_keys(req, object);
        match offset(req, offset_value)? {
            Err(name) if is_list => {
                let _ = name;
                Ok(Owner::null())
            }
            Err(name) => named(req, object, name.as_bytes(), None),
            Ok(index) if is_list => item(req, object, index),
            Ok(index) => {
                if !(0..=i64::from(i32::MAX)).contains(&index) {
                    return Err(throw_error(
                        req,
                        "ValueError",
                        "must be between 0 and 2147483647",
                    ));
                }
                item(req, object, index)
            }
        }
    }

    fn has_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset_value: &Value,
        _not_empty: bool,
    ) -> Result<bool, Thrown> {
        let dims = dims(req, object);
        if dims != Dims::Legacy {
            return match (dims, modern_index(req, offset_value)?) {
                (_, Index::Long(n)) => Ok(n >= 0 && n < length(req, object)),
                (Dims::ModernMap, Index::Str(name)) => {
                    let found = named(req, object, name.as_bytes(), None)?;
                    let present = found.as_object().is_some();
                    found.give_up(req);
                    Ok(present)
                }
                (Dims::Collection, Index::Str(name)) => {
                    let found = collection_named(req, object, name.as_bytes())?;
                    let present = found.as_object().is_some();
                    found.give_up(req);
                    Ok(present)
                }
                _ => Err(illegal_offset(
                    req,
                    object,
                    offset_value,
                    DimensionRead::Silent,
                )),
            };
        }
        let is_list = !named_keys(req, object);
        match offset(req, offset_value)? {
            Err(_) if is_list => Ok(false),
            Err(name) => {
                let found = named(req, object, name.as_bytes(), None)?;
                let present = found.as_object().is_some();
                found.give_up(req);
                Ok(present)
            }
            Ok(index) => Ok(index >= 0 && index < length(req, object)),
        }
    }

    /// `php_dom_get_iterator`: the walk made standing at the first
    /// item, php's `get_item(0)`, and refused by reference.
    fn get_iterator(
        &self,
        req: &mut Request,
        object: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        if by_ref {
            let message = "An iterator cannot be used with foreach by reference";
            return Err(throw_error(req, "Error", message));
        }
        let current = item(req, object, 0)?;
        Ok(Some(Box::new(NodeIter {
            current: object_only(req, current),
            index: 0,
            steps: 0,
        })))
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let map = dom_mut(req, object).and_then(|d| match std::mem::take(&mut d.extra) {
            Extra::Map(m) => Some(m),
            _ => None,
        });
        if let Some(map) = map {
            if let Some(base) = map.base {
                base.give_up(req);
            }
            if let MapKind::NodeSet(array) = map.kind {
                array.give_up(req);
            }
        }
        crate::release_libxml(req, object);
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(Dom {
            extra: Extra::Map(map),
            ..
        }) = dom(req, object)
        {
            if let Some(base) = map.base.as_deref() {
                visit(base.as_value());
            }
            if let MapKind::NodeSet(array) = &map.kind {
                visit(Value::array(**array));
            }
        }
        GcTable::Properties
    }
}

/// `php_dom_iterator`: the item the walk stands at, taken when it is
/// made and each time it moves, one owner held, `curobj`; its index; and
/// how many steps were asked of it, which `InternalIterator::rewind`
/// reads, since `php_dom_iterator_funcs` has no `rewind` of its own.
///
/// A legacy list of a node's children or attributes steps to the node
/// after the one it stands at, so a removal under it ends the walk
/// there; every other list asks for the item at the next index, live:
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<x><a/><b/><c/><d/></x>');
/// foreach ($d->documentElement->childNodes as $i => $n) { echo $i, $n->nodeName, ';'; if ($i === 1) $n->remove(); }
/// $m = Dom\XMLDocument::createFromString('<x><a/><b/><c/><d/></x>');
/// foreach ($m->documentElement->childNodes as $i => $n) { echo $i, $n->nodeName, ';'; if ($i === 1) $n->remove(); }
/// $it = $d->documentElement->childNodes->getIterator();
/// foreach (new IteratorIterator($it) as $n) {}
/// try { $it->rewind(); } catch (Error $e) { echo "\n", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 0a;1b;0a;1b;2d;
/// Iterator does not support rewinding
/// ```
struct NodeIter {
    current: Option<Owner<Value>>,
    index: i64,
    steps: u64,
}

impl ClassIterator for NodeIter {
    fn rewind(&mut self, req: &mut Request, _: PhpObject) -> Result<(), Thrown> {
        if self.steps == 0 {
            return Ok(());
        }
        Err(throw_error(
            req,
            "Error",
            "Iterator does not support rewinding",
        ))
    }

    fn valid(&mut self, _: &mut Request, _: PhpObject) -> Result<bool, Thrown> {
        Ok(self.current.is_some())
    }

    fn current(&mut self, req: &mut Request, _: PhpObject) -> Result<Owner<Value>, Thrown> {
        let current = self.current.as_deref().copied().unwrap_or_default();
        Ok(Owner::take(req, current))
    }

    /// `php_dom_iterator_current_key`: the index, or a named map's item
    /// by its name, a modern attribute by its qualified name.
    fn key(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        if !named_keys(req, object) {
            return Ok(Owner::int(self.index));
        }
        let current = self
            .current
            .as_deref()
            .and_then(php_ext::values::Value::as_object);
        let Some(node) = current.and_then(|o| node_of(req, o)) else {
            return Ok(Owner::null());
        };
        let modern = current.is_some_and(|o| crate::modern::follows_spec(req, o));
        let name = if modern && node.kind() == Kind::Attribute {
            crate::qualified_name(node)
        } else {
            node.name()
        };
        string(req, &name)
    }

    fn move_forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        self.steps += 1;
        let Some(garbage) = self.current.take() else {
            return Ok(());
        };
        self.index += 1;
        let next = match garbage.as_object() {
            Some(old) => self.next(req, object, old),
            None => Ok(Owner::null()),
        };
        // `zval_ptr_dtor(&garbage)`, whatever the step came to.
        garbage.give_up(req);
        self.current = object_only(req, next?);
        Ok(())
    }

    fn release(&mut self, req: &mut Request, _: PhpObject) {
        if let Some(current) = self.current.take() {
            current.give_up(req);
        }
    }
}

impl NodeIter {
    /// The item after `old`: a legacy list of a node's children or
    /// attributes steps to the node after it, every other list asks for
    /// the item at the next index.
    fn next(
        &self,
        req: &mut Request,
        object: PhpObject,
        old: PhpObject,
    ) -> Result<Owner<Value>, Thrown> {
        let cursor = !crate::modern::follows_spec(req, old)
            && matches!(
                dom(req, object),
                Some(Dom { extra: Extra::Map(m), .. })
                    if matches!(m.kind, MapKind::ChildNodes | MapKind::Attributes)
            );
        match view(req, object) {
            View::Nodes { base, .. } if cursor => match node_of(req, old).and_then(Node::next) {
                Some(node) => object_for(req, node, Some(base)),
                None => Ok(Owner::null()),
            },
            _ => item(req, object, self.index),
        }
    }
}

/// The item the walk stands at, kept where it is an object, `curobj`; a
/// `null` past the end given up.
fn object_only(req: &mut Request, item: Owner<Value>) -> Option<Owner<Value>> {
    if item.as_object().is_some() {
        return Some(item);
    }
    item.give_up(req);
    None
}

/// `class DOMNodeList implements IteratorAggregate, Countable`: a live
/// list, its `length` `@virtual`, read through [`HANDLERS`].
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/><b/></r>');
/// $l = $d->documentElement->childNodes;
/// var_dump(count($l), $l->count(), $l->length, $l->item(5), $l->item(-1));
/// foreach ($l as $k => $n) echo $k, '=', $n->nodeName, ' ';
/// echo $l[1]->nodeName, ' ', isset($l[1]) ? 'y' : 'n', isset($l[2]) ? 'y' : 'n', "\n";
/// $i = $l->getIterator();
/// var_dump(get_class($i), $i->current()->nodeName);
/// ```
/// ```expect
/// int(2)
/// int(2)
/// int(2)
/// NULL
/// NULL
/// 0=a 1=b b yn
/// string(16) "InternalIterator"
/// string(1) "a"
/// ```
#[php_class(
    implements(crate::IteratorAggregate, crate::Countable),
    size = 80, beside(88), payload = Dom,
    handlers = HANDLERS
)]
pub(crate) struct DOMNodeList;

#[php_class]
impl DOMNodeList {
    #[php(property, virtual)]
    const length: i64;

    /// `DOMNodeList::count(): int`.
    #[php(tentative)]
    fn count(req: &mut Request, this: This<Self>) -> i64 {
        length(req, this.object())
    }

    /// `DOMNodeList::getIterator(): Iterator`, `zend_create_internal_iterator_zval`.
    fn getIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<crate::Iterator>>, Thrown> {
        php_ext::call::internal_iterator(req, &this.object()).map(Owner::into_instance)
    }

    /// `DOMNodeList::item(int $index)`.
    fn item(
        req: &mut Request,
        this: This<Self>,
        index: i64,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        item(req, this.object(), index).map(Untyped)
    }
}

/// `DOMNamedNodeMap::getNamedItem(string $qualifiedName): ?DOMNode`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r xmlns:p="urn:p" a="1" p:b="2"/>');
/// $m = $d->documentElement->attributes;
/// var_dump($m->length, $m->getNamedItem('a')->value, $m->getNamedItemNS('urn:p', 'b')->value, $m->getNamedItem('z'), $m['a']->value, $m[1]->name);
/// foreach ($m as $k => $v) echo $k, '=', $v->value, ';';
/// try { $m->item(-1); } catch (ValueError $e) { echo "\n", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// string(1) "1"
/// string(1) "2"
/// NULL
/// string(1) "1"
/// string(1) "b"
/// a=1;b=2;
/// DOMNamedNodeMap::item(): Argument #1 ($index) must be between 0 and 2147483647
/// ```
#[php_class(
    implements(crate::IteratorAggregate, crate::Countable),
    size = 80, beside(88), payload = Dom,
    handlers = HANDLERS
)]
pub(crate) struct DOMNamedNodeMap;

#[php_class]
impl DOMNamedNodeMap {
    #[php(property, virtual)]
    const length: i64;

    /// `DOMNamedNodeMap::getNamedItem(string $qualifiedName): ?DOMNode`.
    #[php(tentative)]
    fn getNamedItem(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: PhpString,
    ) -> Result<Owner<Raw<Option<Instance<crate::node::DOMNode>>>>, Thrown> {
        let found = named(req, this.object(), qualifiedName.as_bytes(), None)?;
        crate::returned(found)
    }

    /// `DOMNamedNodeMap::getNamedItemNS(?string $namespace, string $localName): ?DOMNode`:
    /// an empty namespace is none to a document that follows the standard,
    /// and a DTD's default is no attribute of the element.
    ///
    /// ```php
    /// $x = '<!DOCTYPE r [<!ATTLIST r dflt CDATA "dtd">]><r xmlns:q="urn:q" bar="no-ns" q:bar="ns"/>';
    /// foreach ([fn() => (function () use ($x) { $d = new DOMDocument; $d->loadXML($x); return $d; })(), fn() => Dom\XMLDocument::createFromString($x)] as $make) {
    ///     $m = $make()->documentElement->attributes;
    ///     var_dump($m->getNamedItemNS('', 'bar')?->nodeValue, $m->getNamedItemNS('urn:q', 'bar')?->nodeValue, $m->getNamedItemNS('', 'dflt')?->nodeValue);
    /// }
    /// ```
    /// ```expect
    /// NULL
    /// string(2) "ns"
    /// NULL
    /// string(5) "no-ns"
    /// string(2) "ns"
    /// NULL
    /// ```
    #[php(tentative)]
    fn getNamedItemNS(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<PhpString>,
        localName: PhpString,
    ) -> Result<Owner<Raw<Option<Instance<crate::node::DOMNode>>>>, Thrown> {
        let ns = namespace.as_ref().map(PhpString::as_bytes);
        let found = named(req, this.object(), localName.as_bytes(), ns)?;
        crate::returned(found)
    }

    /// `DOMNamedNodeMap::item(int $index): ?DOMNode`.
    #[php(tentative)]
    fn item(
        req: &mut Request,
        this: This<Self>,
        index: i64,
    ) -> Result<Owner<Raw<Option<Instance<crate::node::DOMNode>>>>, Thrown> {
        if !(0..=i64::from(i32::MAX)).contains(&index) {
            return Err(php_ext::raising::arg_value_error(
                req,
                1,
                "be between 0 and 2147483647",
            ));
        }
        let found = item(req, this.object(), index)?;
        crate::returned(found)
    }

    #[php(tentative, implementation = DOMNodeList::count)]
    fn count(this: This<Self>) -> i64;
    #[php(implementation = DOMNodeList::getIterator)]
    fn getIterator(this: This<Self>) -> Instance<crate::Iterator>;
}

/// `class Dom\NodeList`, `DOMNodeList`'s methods as its own
/// (`@implementation-alias`).
#[php_class(
    namespace(Dom),
    implements(crate::IteratorAggregate, crate::Countable),
    size = 80, beside(88), payload = Dom,
    handlers = HANDLERS
)]
pub(crate) struct NodeList;

#[php_class]
impl NodeList {
    #[php(property, virtual)]
    const length: i64;
    #[php(implementation = DOMNodeList::count)]
    fn count(this: This<Self>) -> i64;
    #[php(implementation = DOMNodeList::getIterator)]
    fn getIterator(this: This<Self>) -> Instance<crate::Iterator>;
    #[php(implementation = DOMNodeList::item)]
    fn item(this: This<Self>, index: i64) -> Option<Instance<crate::modern::Node>>;
}

/// `class Dom\NamedNodeMap`, `DOMNamedNodeMap`'s methods as its own.
#[php_class(
    namespace(Dom),
    implements(crate::IteratorAggregate, crate::Countable),
    size = 80, beside(88), payload = Dom,
    handlers = HANDLERS
)]
pub(crate) struct NamedNodeMap;

#[php_class]
impl NamedNodeMap {
    #[php(property, virtual)]
    const length: i64;
    #[php(implementation = DOMNamedNodeMap::item)]
    fn item(this: This<Self>, index: i64) -> Option<Instance<crate::modern::Attr>>;
    #[php(implementation = DOMNamedNodeMap::getNamedItem)]
    fn getNamedItem(
        this: This<Self>,
        qualifiedName: PhpString,
    ) -> Option<Instance<crate::modern::Attr>>;
    #[php(implementation = DOMNamedNodeMap::getNamedItemNS)]
    fn getNamedItemNS(
        this: This<Self>,
        namespace: Option<PhpString>,
        localName: PhpString,
    ) -> Option<Instance<crate::modern::Attr>>;
    #[php(implementation = DOMNodeList::count)]
    fn count(this: This<Self>) -> i64;
    #[php(implementation = DOMNodeList::getIterator)]
    fn getIterator(this: This<Self>) -> Instance<crate::Iterator>;
}

/// `class Dom\DtdNamedNodeMap`, a doctype's entities or notations.
#[php_class(
    namespace(Dom),
    implements(crate::IteratorAggregate, crate::Countable),
    size = 80, beside(88), payload = Dom,
    handlers = HANDLERS
)]
pub(crate) struct DtdNamedNodeMap;

#[php_class]
impl DtdNamedNodeMap {
    #[php(property, virtual)]
    const length: i64;
    #[php(implementation = DOMNamedNodeMap::item)]
    fn item(
        this: This<Self>,
        index: i64,
    ) -> Option<Union<Instance<crate::classes::Entity>, Instance<crate::classes::Notation>>>;
    #[php(implementation = DOMNamedNodeMap::getNamedItem)]
    fn getNamedItem(
        this: This<Self>,
        qualifiedName: PhpString,
    ) -> Option<Union<Instance<crate::classes::Entity>, Instance<crate::classes::Notation>>>;
    #[php(implementation = DOMNamedNodeMap::getNamedItemNS)]
    fn getNamedItemNS(
        this: This<Self>,
        namespace: Option<PhpString>,
        localName: PhpString,
    ) -> Option<Union<Instance<crate::classes::Entity>, Instance<crate::classes::Notation>>>;
    #[php(implementation = DOMNodeList::count)]
    fn count(this: This<Self>) -> i64;
    #[php(implementation = DOMNodeList::getIterator)]
    fn getIterator(this: This<Self>) -> Instance<crate::Iterator>;
}

/// `class Dom\HTMLCollection`, a live list of elements, named items
/// looked up by `id` and `name`.
#[php_class(
    namespace(Dom),
    implements(crate::IteratorAggregate, crate::Countable),
    size = 80, beside(88), payload = Dom,
    handlers = HANDLERS
)]
pub(crate) struct HTMLCollection;

#[php_class]
impl HTMLCollection {
    #[php(property, virtual)]
    const length: i64;
    #[php(implementation = DOMNodeList::item)]
    fn item(this: This<Self>, index: i64) -> Option<Instance<crate::element::Element>>;

    /// `Dom\HTMLCollection::namedItem(string $key): ?Dom\Element`. php's
    /// walk of a child-element collection never moves past the first child
    /// element (`dom_map_collection_named_item_elements_iter` advances only
    /// from a candidate that is not the first child), so a key the first
    /// element does not carry loops forever there; this walk moves on.
    ///
    /// ```php
    /// $d = Dom\HTMLDocument::createEmpty();
    /// $r = $d->appendChild($d->createElement('div'));
    /// $a = $r->appendChild($d->createElement('p'));
    /// $a->setAttribute('name', 'n');
    /// $a->setAttribute('id', 'i');
    /// $c = $r->children;
    /// var_dump($c->length, $c->namedItem('n') === $a, $c['i'] === $a, $c->namedItem(''), $c[0] === $a, isset($c['n']));
    /// ```
    /// ```expect
    /// int(1)
    /// bool(true)
    /// bool(true)
    /// NULL
    /// bool(true)
    /// bool(true)
    /// ```
    fn namedItem(
        req: &mut Request,
        this: This<Self>,
        key: PhpString,
    ) -> Result<Owner<Raw<Option<Instance<crate::element::Element>>>>, Thrown> {
        let found = collection_named(req, this.object(), key.as_bytes())?;
        crate::returned(found)
    }

    #[php(implementation = DOMNodeList::count)]
    fn count(this: This<Self>) -> i64;
    #[php(implementation = DOMNodeList::getIterator)]
    fn getIterator(this: This<Self>) -> Instance<crate::Iterator>;
}
