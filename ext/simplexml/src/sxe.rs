//! The object model of `simplexml.c`: one payload for `SimpleXMLElement`
//! and every class derived from it ([`Sxe`], `php_sxe_object`), the walk
//! each object makes from its node ([`Walk`], `SXE_ITER`), and the
//! handler table php installs on them (`sxe_object_handlers`), each slot
//! the Rust of its C function.

use php_ext::abi::{
    ClassIterator, CloneObj, DimensionRead, GcTable, ObjectHandlers, PropertyPlace, PropertyRead,
    PropertyTest, Std,
};
use php_ext::call::{call_method, lookup_class, new_object};
use php_ext::coerce::{to_double, to_long, to_number, to_string, value_name, with_string};
use php_ext::raising::{Level, Thrown, docref, throw_error, throw_error_chained};
use php_ext::request::Request;
use php_ext::values::walk::{self};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, TypeTag, Value, ValueRef};
use php_ext_libxml::ffi::{Borrowed, Kind, Node, NodeObject, XPath};

/// `SXE_ITER`: what an object walks from the node it holds, matched
/// exhaustively wherever php switches on `iter.type`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Walk {
    /// `SXE_ITER_NONE`: the node itself.
    #[default]
    Node,
    /// `SXE_ITER_ELEMENT`: the node's child elements of one name.
    Elements(Vec<u8>),
    /// `SXE_ITER_CHILD`: every child element.
    Children,
    /// `SXE_ITER_ATTRLIST`: the node's attributes, those of one name
    /// where a name is given.
    Attributes(Option<Vec<u8>>),
}

/// `iter.nsprefix` and `iter.isprefix`: the namespace a walk keeps to,
/// by its URI or, where `is_prefix`, by its prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ns {
    pub(crate) name: Vec<u8>,
    pub(crate) is_prefix: bool,
}

impl Ns {
    /// The filter a non-empty name makes, php's `nsprefix &&
    /// *ZSTR_VAL(nsprefix)`; none for an empty one.
    pub(crate) fn of(name: Option<&[u8]>, is_prefix: bool) -> Option<Ns> {
        name.filter(|n| !n.is_empty()).map(|n| Ns {
            name: n.to_vec(),
            is_prefix,
        })
    }
}

/// `php_sxe_object`: the libxml node object that names the node and counts
/// its document, the walk, the element its own iteration stands at
/// (`iter.data`), its `XPath` evaluator and the table its properties were
/// last read into (`properties`).
#[derive(Default)]
pub(crate) struct SxeState {
    pub(crate) obj: NodeObject,
    pub(crate) walk: Walk,
    pub(crate) ns: Option<Ns>,
    /// `iter.data`, the object's owner of it.
    pub(crate) current: Option<Owner<PhpObject>>,
    pub(crate) xpath: Option<XPath>,
    /// `sxe->properties`, the object's owner of it: kept until the next
    /// read or the object's free, so what it holds lives as long as
    /// php's does.
    pub(crate) properties: Option<Owner<PhpArray>>,
    /// `sxe->tmp`, the object's owner of it: the element a write through
    /// a missing name made, kept until the next such write or the
    /// object's free.
    pub(crate) made: Option<Owner<PhpObject>>,
}

/// The payload: `php_sxe_object`'s fields, boxed, inside the `sizeof` php
/// allocates (design §5.13).
pub(crate) type Sxe = Box<SxeState>;

/// The payload of `object`.
pub(crate) fn sxe(req: &Request, object: PhpObject) -> Option<&Sxe> {
    object.native::<Sxe>(req)
}

/// The payload of `object`, to write.
pub(crate) fn sxe_mut(req: &mut Request, object: PhpObject) -> Option<&mut Sxe> {
    object.native_mut::<Sxe>(req)
}

/// The object's libxml half borrowed for a call that takes the request
/// beside it ([`NodeObject::borrow`]).
pub(crate) fn libxml_half(req: &mut Request, object: PhpObject) -> Option<Borrowed> {
    sxe_mut(req, object).map(|s| s.obj.borrow())
}

/// What a walk reads off one object: its node, walk and namespace, copied
/// so the request is free for the calls the walk makes.
#[derive(Debug, Clone, Default)]
pub(crate) struct View {
    pub(crate) node: Option<Node>,
    pub(crate) walk: Walk,
    pub(crate) ns: Option<Ns>,
}

impl View {
    pub(crate) fn of(req: &Request, object: PhpObject) -> View {
        sxe(req, object).map_or_else(View::default, |s| View {
            node: s.obj.bound(),
            walk: s.walk.clone(),
            ns: s.ns.clone(),
        })
    }

    /// `GET_NODE`: the node, or php's `Error` for an object nothing
    /// initialised.
    pub(crate) fn node(&self, req: &mut Request) -> Result<Node, Thrown> {
        self.node.ok_or_else(|| {
            throw_error(req, "Error", "SimpleXMLElement is not properly initialized")
        })
    }

    /// `match_ns`.
    pub(crate) fn matches_ns(&self, node: Node) -> bool {
        match_ns(node, self.ns.as_ref())
    }

    /// `php_sxe_get_first_node_non_destructive`: the first node of the
    /// walk from `node`, or `node` itself for the walk of none.
    pub(crate) fn first(&self, node: Node) -> Option<Node> {
        match self.walk {
            Walk::Node => Some(node),
            Walk::Elements(_) | Walk::Children | Walk::Attributes(_) => self.reset(node),
        }
    }

    /// `php_sxe_reset_iterator_no_clear_iter_data`: the walk's first node
    /// under `node`.
    pub(crate) fn reset(&self, node: Node) -> Option<Node> {
        let start = match self.walk {
            Walk::Attributes(_) => node.properties(),
            Walk::Node | Walk::Elements(_) | Walk::Children => node.children(),
        };
        self.fetch(start)
    }

    /// `php_sxe_iterator_fetch`: the first node from `at` on the walk
    /// takes.
    pub(crate) fn fetch(&self, mut at: Option<Node>) -> Option<Node> {
        while let Some(n) = at {
            let taken = match &self.walk {
                Walk::Attributes(name) => {
                    n.kind() == Kind::Attribute
                        && name.as_ref().is_none_or(|x| n.name() == *x)
                        && self.matches_ns(n)
                }
                Walk::Elements(name) => {
                    n.kind() == Kind::Element && n.name() == *name && self.matches_ns(n)
                }
                Walk::Node | Walk::Children => n.kind() == Kind::Element && self.matches_ns(n),
            };
            if taken {
                return Some(n);
            }
            at = n.next();
        }
        None
    }

    /// `sxe_get_element_by_offset`: the element at `offset` of the walk
    /// from `node`, and how many came before where it stopped.
    fn by_offset(&self, offset: i64, node: Option<Node>) -> (Option<Node>, i64) {
        if self.walk == Walk::Node {
            return (if offset == 0 { node } else { None }, 0);
        }
        let mut at = node;
        let mut index = 0;
        while let Some(n) = at {
            if index > offset {
                break;
            }
            if n.kind() == Kind::Element && self.matches_ns(n) {
                let counted = match &self.walk {
                    Walk::Children => true,
                    Walk::Elements(name) => n.name() == *name,
                    Walk::Node | Walk::Attributes(_) => false,
                };
                if counted {
                    if index == offset {
                        break;
                    }
                    index += 1;
                }
            }
            at = n.next();
        }
        (at, index)
    }

    /// `sxe_find_element_by_name`.
    fn find(&self, mut at: Option<Node>, name: &[u8]) -> Option<Node> {
        while let Some(n) = at {
            if n.kind() == Kind::Element && self.matches_ns(n) && n.name() == name {
                return Some(n);
            }
            at = n.next();
        }
        None
    }

    /// `sxe_get_element_by_name`: whether `name` is a child element where a
    /// write through the object would reach it.
    fn has_element(&self, node: Node, name: &[u8]) -> bool {
        let mut at = match self.walk {
            Walk::Attributes(_) => Some(node),
            Walk::Node | Walk::Children => {
                let children = View {
                    walk: Walk::Children,
                    ..self.clone()
                };
                children.reset(node)
            }
            Walk::Elements(_) => self.first(node),
        };
        if let Walk::Elements(own) = &self.walk {
            match self.find(at, own) {
                Some(found) => at = found.children(),
                None => return false,
            }
        }
        self.find(at, name).is_some()
    }

    /// The attributes a named access reads: `(first, test)`, php's `attr`
    /// and `test`, or `None` where the access is the walk of children.
    fn attrs(&self, node: Node) -> Option<(Option<Node>, Option<Node>)> {
        match self.walk {
            Walk::Attributes(_) => {
                let first = self.first(node);
                Some((first, first))
            }
            Walk::Children => None,
            Walk::Node | Walk::Elements(_) => {
                let first = self.first(node);
                Some((first, first.and_then(Node::properties)))
            }
        }
    }

    /// The attributes from `attr` on this walk counts: of its name where a
    /// named attribute walk has one (`test`), in its namespace.
    fn attr_counts(&self, attr: Node) -> bool {
        let named = match &self.walk {
            Walk::Attributes(Some(name)) => attr.name() == *name,
            Walk::Attributes(None) | Walk::Node | Walk::Elements(_) | Walk::Children => true,
        };
        named && self.matches_ns(attr)
    }

    /// The attribute at `index` of those the walk counts from `attr`, and
    /// how many it counted.
    fn attr_at(&self, mut attr: Option<Node>, index: i64) -> (Option<Node>, i64) {
        let mut at = 0;
        while let Some(a) = attr {
            if at > index {
                break;
            }
            if self.attr_counts(a) {
                if at == index {
                    return (Some(a), at);
                }
                at += 1;
            }
            attr = a.next();
        }
        (None, at)
    }

    /// The first attribute from `attr` the walk counts named `name`.
    fn attr_named(&self, mut attr: Option<Node>, name: &[u8]) -> Option<Node> {
        while let Some(a) = attr {
            if self.attr_counts(a) && a.name() == name {
                return Some(a);
            }
            attr = a.next();
        }
        None
    }
}

/// `match_ns`: no filter takes a node with no namespace or an unprefixed
/// one; a filter takes a node whose namespace has that URI or prefix.
pub(crate) fn match_ns(node: Node, ns: Option<&Ns>) -> bool {
    let own = node.ns();
    match ns {
        None => own.is_none_or(|n| n.prefix().is_none()),
        Some(f) => own.is_some_and(|n| {
            let named = if f.is_prefix { n.prefix() } else { n.href() };
            named.as_deref() == Some(f.name.as_slice())
        }),
    }
}

/// `node_as_zval`: a fresh object of `of`'s class standing for `node` under
/// `walk`, sharing `of`'s document.
pub(crate) fn wrap(
    req: &mut Request,
    of: PhpObject,
    node: Node,
    walk: Walk,
    ns: Option<Ns>,
) -> Result<Owner<PhpObject>, Thrown> {
    let shared = sxe(req, of).and_then(|s| s.obj.shared_doc());
    let made = new_object(req, of.class())?;
    if let Some(mut obj) = libxml_half(req, *made) {
        if let (Some(shared), Some(doc)) = (shared, node.doc()) {
            obj.share(req, Some(shared), doc);
        }
        obj.hold_node(req, node);
    }
    if let Some(s) = sxe_mut(req, *made) {
        s.walk = walk;
        s.ns = ns;
    }
    Ok(made)
}

/// `sxe_unlink_node`: `node` taken out of its tree, and freed where no
/// object holds it.
fn unlink(req: &mut Request, node: Node) {
    node.unlink();
    if !node.is_named() {
        node.free_resource(req);
    }
}

/// `change_node_zval`: `node`'s content set to `value` read as text.
fn set_text(node: Node, value: &[u8]) {
    if let Some(encoded) = node.encode_entities(value) {
        node.set_content(&encoded);
    }
}

/// `sxe_access_mode`: whether a handler was reached as a property or as a
/// dimension, before the walk and the member settle it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Elements,
    Attributes,
}

/// The member a property or dimension handler is handed: `$o[]`'s none,
/// an integer offset, or a name.
#[derive(Debug, Clone)]
enum Member {
    Append,
    Index(i64),
    Name(Vec<u8>),
}

impl Member {
    /// The member of a dimension handler, a value that is neither an
    /// integer nor a string read as a string (`zval_try_get_string_func`),
    /// trimmed as php's write trims it where `trim`.
    fn of(req: &mut Request, offset: Option<&Value>, trim: bool) -> Result<Member, Thrown> {
        let Some(offset) = offset else {
            return Ok(Member::Append);
        };
        let offset = offset.deref();
        match offset.as_ref() {
            ValueRef::Int(n) => Ok(Member::Index(n)),
            ValueRef::Str(s) => Ok(Member::Name(s.as_bytes().to_vec())),
            _ => {
                let name = with_string(req, &offset, |bytes| {
                    if trim { trimmed(bytes) } else { bytes.to_vec() }
                })?;
                Ok(Member::Name(name))
            }
        }
    }
}

/// `php_trim(str, NULL, 0, 3)`: php's default whitespace off both ends.
fn trimmed(bytes: &[u8]) -> Vec<u8> {
    let space = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0);
    let start = bytes.iter().position(|b| !space(b)).unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !space(b))
        .map_or(start, |e| e + 1);
    bytes[start..end].to_vec()
}

/// A C string's bytes, as `xmlStrEqual` and libxml read a name: up to
/// its first NUL.
pub(crate) fn c_name(bytes: &[u8]) -> Vec<u8> {
    php_ext::coerce::c_str(bytes).to_vec()
}

/// php's `type` argument to the read handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fetch {
    Read,
    Silent,
    Write,
}

/// `sxe_prop_dim_read`.
fn prop_dim_read(
    req: &mut Request,
    object: PhpObject,
    member: Member,
    mut access: Access,
    fetch: Fetch,
) -> Result<Owner<Value>, Thrown> {
    let view = View::of(req, object);
    match member {
        Member::Append if matches!(view.walk, Walk::Attributes(_)) => {
            return Err(throw_error(req, "Error", "Cannot create unnamed attribute"));
        }
        Member::Append => access = Access::Elements,
        Member::Index(_) if !matches!(view.walk, Walk::Attributes(_)) => access = Access::Elements,
        Member::Index(_) | Member::Name(_) => {}
    }
    let base = view.node(req)?;
    let mut node = Some(base);
    let mut attr = None;
    if let Some((first, first_attr)) = view.attrs(base) {
        if matches!(view.walk, Walk::Attributes(_)) {
            access = Access::Attributes;
        } else if matches!(member, Member::Append)
            && first
                .and_then(Node::parent)
                .is_some_and(|p| p.kind() == Kind::Document)
        {
            return Err(throw_error(req, "Error", "Cannot create unnamed attribute"));
        }
        node = first;
        attr = first_attr;
    }
    let Some(node) = node else {
        return Ok(Owner::null());
    };
    if access == Access::Attributes {
        let found = match (&member, &view.walk) {
            (Member::Index(i), Walk::Attributes(_)) => view.attr_at(attr, *i).0,
            (Member::Index(_) | Member::Append, _) => None,
            (Member::Name(name), _) => view.attr_named(attr, name),
        };
        return match found {
            Some(a) => Ok(wrap(req, object, a, Walk::Node, view.ns.clone())?.into_value()),
            None => Ok(Owner::null()),
        };
    }
    match member {
        Member::Name(name) => {
            if fetch == Fetch::Silent && view.find(node.children(), &name).is_none() {
                return Ok(Owner::null());
            }
            Ok(wrap(req, object, node, Walk::Elements(name), view.ns.clone())?.into_value())
        }
        Member::Append | Member::Index(_) => {
            let mine = node;
            let from = if view.walk == Walk::Children {
                view.first(node)
            } else {
                Some(node)
            };
            let (found, count) = match member {
                _ if view.walk == Walk::Node => {
                    if let Member::Index(i) = member
                        && i > 0
                    {
                        past_the_end(req, mine, i, 0);
                    }
                    (from, 0)
                }
                Member::Index(i) => view.by_offset(i, from),
                Member::Append | Member::Name(_) => (None, 0),
            };
            if let Some(n) = found {
                return Ok(wrap(req, object, n, Walk::Node, view.ns.clone())?.into_value());
            }
            if fetch != Fetch::Write {
                return Ok(Owner::null());
            }
            if let Member::Index(i) = member
                && count < i
            {
                past_the_end(req, mine, i, count);
            }
            let made = mine
                .parent()
                .and_then(|p| p.new_text_child(mine.ns(), &mine.name(), None));
            match made {
                Some(n) => Ok(wrap(req, object, n, Walk::Node, view.ns.clone())?.into_value()),
                None => Ok(Owner::null()),
            }
        }
    }
}

/// The text a value assigned to a node is, `sxe_prop_dim_write`'s
/// `value_str`, or php's `TypeError` for a value with none.
fn text_of(req: &mut Request, value: &Value, access: Access) -> Result<Vec<u8>, Thrown> {
    match complex(req, value, access) {
        Some(message) => Err(throw_error(req, "TypeError", &message)),
        None => plain_text(req, value),
    }
}

/// php's `TypeError` text for a value that has no text to assign, a
/// complex type; `None` for one that has.
fn complex(req: &mut Request, value: &Value, access: Access) -> Option<String> {
    let value = value.deref();
    let exact = match value.as_ref() {
        ValueRef::Object(o) => lookup_class(req, b"SimpleXMLElement")
            .ok()
            .flatten()
            .is_some_and(|c| c == o.class()),
        _ => false,
    };
    match value.as_ref() {
        ValueRef::Int(_)
        | ValueRef::Bool(_)
        | ValueRef::Float(_)
        | ValueRef::Null
        | ValueRef::Undef
        | ValueRef::Str(_) => None,
        ValueRef::Object(_) if exact => None,
        _ => {
            let target = match access {
                Access::Attributes => "attributes",
                Access::Elements => "properties",
            };
            let given = value_name(req, &value);
            Some(format!(
                "It's not possible to assign a complex type to {target}, {given} given"
            ))
        }
    }
}

/// The text of a value [`complex`] took: a scalar as a string, a
/// `SimpleXMLElement` its cast.
fn plain_text(req: &mut Request, value: &Value) -> Result<Vec<u8>, Thrown> {
    let value = value.deref();
    if let Some(o) = value.as_object() {
        return Ok(cast_text(req, o)?.unwrap_or_default());
    }
    with_string(req, &value, <[u8]>::to_vec)
}

/// php's warning for an element written past the ones there are.
fn past_the_end(req: &mut Request, node: Node, index: i64, count: i64) {
    let name = String::from_utf8_lossy(&node.name()).into_owned();
    docref(
        req,
        Level::Warning,
        &format!("Cannot add element {name} number {index} when only {count} such elements exist"),
    );
}

/// What a write found where it looked: the node its member names where
/// exactly one does, how many did, and how far the count got.
struct Hits {
    found: Option<Node>,
    count: usize,
    seen: i64,
}

/// `sxe_prop_dim_write`'s search: the attribute or the element `member`
/// names under `mine`, `attr` the attributes the walk reads.
fn hits(
    req: &mut Request,
    view: &View,
    member: &Member,
    access: Access,
    mine: Node,
    attr: Option<Node>,
) -> Result<Hits, Thrown> {
    let mut hits = Hits {
        found: None,
        count: 0,
        seen: 0,
    };
    let hit = match (access, member) {
        (Access::Attributes, Member::Index(i)) => {
            let (hit, seen) = view.attr_at(attr, *i);
            hits.seen = seen;
            hit
        }
        (Access::Attributes, Member::Name(name)) => view.attr_named(attr, name),
        (Access::Attributes, Member::Append) => None,
        (Access::Elements, Member::Append | Member::Index(_)) => {
            if mine.kind() == Kind::Attribute {
                return Err(throw_error(
                    req,
                    "Error",
                    "Cannot create duplicate attribute",
                ));
            }
            match member {
                _ if view.walk == Walk::Node => {
                    if let Member::Index(i) = member
                        && *i > 0
                    {
                        past_the_end(req, mine, *i, 0);
                    }
                    Some(mine)
                }
                Member::Index(i) => {
                    let (hit, seen) = view.by_offset(*i, Some(mine));
                    hits.seen = seen;
                    hit
                }
                Member::Append | Member::Name(_) => None,
            }
        }
        (Access::Elements, Member::Name(name)) => {
            let mut at = mine.children();
            while let Some(n) = at {
                if n.kind() != Kind::Text && n.name() == *name && view.matches_ns(n) {
                    hits.found = Some(n);
                    hits.count += 1;
                }
                at = n.next();
            }
            return Ok(hits);
        }
    };
    if hit.is_some() {
        hits.found = hit;
        hits.count = 1;
    }
    Ok(hits)
}

/// The access a write through `member` makes, php's `access_mode` after
/// its first checks: `$o[]` of an attribute list and an empty name are
/// php's refusals.
fn write_access(
    req: &mut Request,
    view: &View,
    member: &Member,
    access: Access,
) -> Result<Access, Thrown> {
    match member {
        Member::Append if matches!(view.walk, Walk::Attributes(_)) => Err(throw_error(
            req,
            "Error",
            "Cannot append to an attribute list",
        )),
        Member::Append | Member::Index(_) if !matches!(view.walk, Walk::Attributes(_)) => {
            Ok(Access::Elements)
        }
        Member::Name(name) if name.is_empty() => {
            let what = match access {
                Access::Attributes => "attribute",
                Access::Elements => "element",
            };
            Err(throw_error(
                req,
                "ValueError",
                &format!("Cannot create {what} with an empty name"),
            ))
        }
        Member::Append | Member::Index(_) | Member::Name(_) => Ok(access),
    }
}

/// `sxe_prop_dim_write`: the node written, or the one made, where any.
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, "\n"; }
/// $x = (new ReflectionClass('SimpleXMLElement'))->newInstanceWithoutConstructor();
/// pk(function () use ($x) { try { $x->a = []; } catch (TypeError $e) {} });
/// try { $x->a = []; } catch (TypeError $e) { echo $e->getMessage(), " / ", $e->getPrevious()->getMessage(), "\n"; }
/// ```
/// ```expect
/// 3576
/// It's not possible to assign a complex type to properties, array given / SimpleXMLElement is not properly initialized
/// ```
///
/// An attribute list writes onto the element it is of, which makes an
/// attribute there where it has none of the name:
///
/// ```php
/// $x = simplexml_load_string('<r/>');
/// $x->attributes()['a'] = '1';
/// $x->attributes()->b = '2';
/// $x->attributes()['a'] = '3';
/// echo $x->asXML();
/// ```
/// ```expect
/// <?xml version="1.0"?>
/// <r a="3" b="2"/>
/// ```
fn prop_dim_write(
    req: &mut Request,
    object: PhpObject,
    member: &Member,
    value: Option<&Value>,
    mut access: Access,
) -> Result<Option<Node>, Thrown> {
    let view = View::of(req, object);
    access = write_access(req, &view, member, access)?;
    let base = match view.node(req) {
        Ok(base) => base,
        // php's `GET_NODE` leaves its `Error` pending and goes on to the
        // value, whose `TypeError` (`zend_type_error`, its `zend_vspprintf`
        // buffer standing across the throw) then chains it as its previous.
        Err(uninitialised) => {
            return Err(match value.and_then(|v| complex(req, v, access)) {
                Some(message) => throw_error_chained(req, uninitialised, "TypeError", &message),
                None => uninitialised,
            });
        }
    };
    let mut node = Some(base);
    let mut attr = None;
    if let Some((first, first_attr)) = view.attrs(base) {
        attr = first_attr;
        if matches!(view.walk, Walk::Attributes(_)) {
            // php 8.5.12 keeps the element an attribute list is of, so a
            // new attribute is made on it, with or without attributes
            // there already.
            access = Access::Attributes;
        } else {
            node = first;
            if matches!(member, Member::Append)
                && first
                    .and_then(Node::parent)
                    .is_some_and(|p| p.kind() == Kind::Document)
            {
                return Err(throw_error(
                    req,
                    "ValueError",
                    "Cannot append to an attribute list",
                ));
            }
            if let (Access::Attributes, None, Walk::Elements(name)) = (access, first, &view.walk) {
                node = base.new_child(base.ns(), name, None);
                attr = node.and_then(Node::properties);
            }
        }
    }
    let text = value.map(|v| text_of(req, v, access)).transpose()?;
    let Some(mine) = node else {
        return Ok(None);
    };
    mine.invalidate_document_lists();
    let hits = hits(req, &view, member, access, mine, attr)?;
    if hits.count == 1 {
        if let (Some(n), Some(text)) = (hits.found, &text) {
            while let Some(child) = n.children() {
                unlink(req, child);
            }
            set_text(n, text);
        }
        return Ok(hits.found);
    }
    if hits.count > 1 {
        docref(
            req,
            Level::Warning,
            "Cannot assign to an array of nodes (duplicate subnodes or attr detected)",
        );
        return Ok(None);
    }
    let text = text.as_deref();
    Ok(match (access, member) {
        (Access::Elements, Member::Name(name)) => mine.new_text_child(None, name, text),
        (Access::Elements, Member::Append | Member::Index(_)) => {
            if let Member::Index(i) = member
                && hits.seen < *i
            {
                past_the_end(req, mine, *i, hits.seen);
            }
            mine.parent()
                .and_then(|p| p.new_text_child(mine.ns(), &mine.name(), text))
        }
        (Access::Attributes, Member::Index(i)) => {
            let seen = hits.seen;
            docref(
                req,
                Level::Warning,
                &format!("Cannot change attribute number {i} when only {seen} attributes exist"),
            );
            None
        }
        (Access::Attributes, Member::Name(name)) => mine.new_ns_prop(None, name, text),
        (Access::Attributes, Member::Append) => None,
    })
}

/// `sxe_prop_dim_exists`: `test` is `isset`, `empty` or `property_exists`.
fn prop_dim_exists(
    req: &mut Request,
    object: PhpObject,
    member: &Member,
    test: PropertyTest,
    mut access: Access,
) -> Result<bool, Thrown> {
    let view = View::of(req, object);
    let base = view.node(req)?;
    let mut node = Some(base);
    if let Member::Index(_) = member
        && !matches!(view.walk, Walk::Attributes(_))
    {
        access = Access::Elements;
        if view.walk == Walk::Children {
            node = view.first(base);
        }
    }
    let mut attr = None;
    if let Some(n) = node
        && let Some((first, first_attr)) = view.attrs(n)
    {
        if matches!(view.walk, Walk::Attributes(_)) {
            access = Access::Attributes;
        }
        node = first;
        attr = first_attr;
    }
    let Some(node) = node else {
        return Ok(false);
    };
    let empty = |content: Option<Vec<u8>>| content.is_none_or(|c| c.is_empty() || c == b"0");
    if access == Access::Attributes {
        let found = match &member {
            Member::Index(i) => view.attr_at(attr, *i).0,
            Member::Name(name) => view.attr_named(attr, name),
            Member::Append => None,
        };
        return Ok(found.is_some_and(|a| {
            test != PropertyTest::NotEmpty || !empty(a.children().and_then(Node::content))
        }));
    }
    let found = match &member {
        Member::Index(i) => {
            let from = if view.walk == Walk::Children {
                view.first(node)
            } else {
                Some(node)
            };
            view.by_offset(*i, from).0
        }
        Member::Name(name) => view.find(node.children(), name),
        Member::Append => None,
    };
    Ok(found.is_some_and(|n| {
        test != PropertyTest::NotEmpty
            || n.children().is_some_and(|c| {
                !(c.kind() == Kind::Text && c.next().is_none() && empty(c.content()))
            })
    }))
}

/// `sxe_prop_dim_delete`.
fn prop_dim_delete(
    req: &mut Request,
    object: PhpObject,
    member: &Member,
    mut access: Access,
) -> Result<(), Thrown> {
    let view = View::of(req, object);
    let base = view.node(req)?;
    let mut node = Some(base);
    if let Member::Index(_) = member
        && !matches!(view.walk, Walk::Attributes(_))
    {
        access = Access::Elements;
        if view.walk == Walk::Children {
            node = view.first(base);
        }
    }
    let mut attr = None;
    if let Some(n) = node
        && let Some((first, first_attr)) = view.attrs(n)
    {
        if matches!(view.walk, Walk::Attributes(_)) {
            access = Access::Attributes;
        }
        node = first;
        attr = first_attr;
    }
    let Some(node) = node else {
        return Ok(());
    };
    node.invalidate_document_lists();
    if access == Access::Attributes {
        let found = match &member {
            Member::Index(i) => view.attr_at(attr, *i).0,
            Member::Name(name) => view.attr_named(attr, name),
            Member::Append => None,
        };
        if let Some(a) = found {
            unlink(req, a);
        }
        return Ok(());
    }
    match &member {
        Member::Index(i) => {
            let from = if view.walk == Walk::Children {
                view.first(node)
            } else {
                Some(node)
            };
            if let Some(n) = view.by_offset(*i, from).0 {
                unlink(req, n);
            }
        }
        Member::Name(name) => {
            let mut at = node.children();
            while let Some(n) = at {
                at = n.next();
                if n.kind() != Kind::Text && n.name() == *name && view.matches_ns(n) {
                    unlink(req, n);
                }
            }
        }
        Member::Append => {}
    }
    Ok(())
}

// ---- the property tables

/// One row of the table `sxe_get_prop_hash` builds, in order: a name and
/// every value `sxe_properties_add` gave it, or the next integer key's
/// value.
enum Row {
    Named(Vec<u8>, Vec<Owner<Value>>),
    Next(Owner<Value>),
}

/// `sxe_properties_add`.
fn add_row(rows: &mut Vec<Row>, name: &[u8], value: Owner<Value>) {
    for row in rows.iter_mut() {
        if let Row::Named(n, values) = row
            && n == name
        {
            values.push(value);
            return;
        }
    }
    rows.push(Row::Named(name.to_vec(), vec![value]));
}

/// The rows as one array: a name given more than once is a list of its
/// values.
fn table(req: &mut Request, rows: Vec<Row>) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for row in rows {
        match row {
            Row::Named(name, mut values) => {
                let key = PhpString::new(req, &name)?.into_key();
                let value = if values.len() == 1 {
                    values.remove(0)
                } else {
                    let mut list = PhpArray::new(req)?;
                    for v in values {
                        list.append(req, v)?;
                    }
                    list.into_value()
                };
                out.set(req, key, value)?;
            }
            Row::Next(value) => out.append(req, value)?,
        }
    }
    Ok(out)
}

/// A string value of `bytes`.
pub(crate) fn string(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// `get_base_node_value`: a node's text where its first child is text that
/// is not blank, an object standing for it otherwise.
fn base_value(
    req: &mut Request,
    of: PhpObject,
    node: Node,
    ns: Option<&Ns>,
) -> Result<Owner<Value>, Thrown> {
    if let Some(c) = node.children()
        && c.kind() == Kind::Text
        && !c.is_blank()
    {
        return string(req, &node.list_string().unwrap_or_default());
    }
    Ok(wrap(req, of, node, Walk::Node, ns.cloned())?.into_value())
}

/// `sxe_get_prop_hash`: the object's attributes under `@attributes`, then
/// its children by name, or its text, as `var_dump` (`debug`) and the
/// property readers see them.
fn prop_hash(req: &mut Request, object: PhpObject, debug: bool) -> Result<Vec<Row>, Thrown> {
    let view = View::of(req, object);
    let mut rows = Vec::new();
    let base = view.node(req)?;
    if debug || view.walk != Walk::Children {
        let node = match view.walk {
            Walk::Elements(_) => view.first(base),
            Walk::Node | Walk::Children | Walk::Attributes(_) => Some(base),
        };
        if let Some(node) = node.filter(|n| n.kind() != Kind::EntityDecl) {
            let mut attrs = Vec::new();
            let mut at = node.properties();
            while let Some(a) = at {
                let named = match &view.walk {
                    Walk::Attributes(Some(name)) => a.name() == *name,
                    Walk::Attributes(None) | Walk::Node | Walk::Elements(_) | Walk::Children => {
                        true
                    }
                };
                if named && view.matches_ns(a) {
                    attrs.push((a.name(), a.list_string().unwrap_or_default()));
                }
                at = a.next();
            }
            if !attrs.is_empty() {
                let mut list = PhpArray::new(req)?;
                for (name, value) in attrs {
                    let value = string(req, &value)?;
                    list.set_str(req, &name, value)?;
                }
                add_row(&mut rows, b"@attributes", list.into_value());
            }
        }
    }
    let Some(node) = view.first(base) else {
        return Ok(rows);
    };
    if matches!(view.walk, Walk::Attributes(_)) {
        return Ok(rows);
    }
    if node.kind() == Kind::Attribute {
        let text = node.list_string().unwrap_or_default();
        rows.push(Row::Next(string(req, &text)?));
        return Ok(rows);
    }
    let mut use_iter = false;
    let mut at = if view.walk == Walk::Children {
        Some(node)
    } else if view.walk == Walk::Node
        || node.children().is_none()
        || node.parent().is_none()
        || node.next().is_none()
        || node.children().and_then(Node::next).is_some()
        || node.children().and_then(Node::children).is_some()
        || node.parent().and_then(Node::children) == node.parent().and_then(Node::last)
    {
        node.children()
    } else {
        use_iter = true;
        view.reset(base)
    };
    while let Some(n) = at {
        let taken =
            if n.children().is_some() || n.prev().is_some() || n.next().is_some() || n.is_blank() {
                n.kind() != Kind::Text
            } else if n.kind() == Kind::Text {
                if n.content().is_some_and(|c| !c.is_empty()) {
                    let text = n.content().unwrap_or_default();
                    rows.push(Row::Next(string(req, &text)?));
                }
                false
            } else {
                true
            };
        if taken && (n.kind() != Kind::Element || view.matches_ns(n)) && n.has_name() {
            let value = base_value(req, object, n, view.ns.as_ref())?;
            if use_iter {
                rows.push(Row::Next(value));
            } else {
                add_row(&mut rows, &n.name(), value);
            }
        }
        if n.kind() == Kind::EntityDecl {
            break;
        }
        at = if use_iter {
            view.fetch(n.next())
        } else {
            n.next()
        };
    }
    Ok(rows)
}

/// `sxe_prop_is_empty`: whether the object has no attribute and no child
/// its walk takes, what its truth is when its walk has no first node.
fn prop_is_empty(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    let view = View::of(req, object);
    let base = view.node(req)?;
    let node = match view.walk {
        Walk::Elements(_) => view.first(base),
        Walk::Node | Walk::Children | Walk::Attributes(_) => Some(base),
    };
    if let Some(node) = node.filter(|n| n.kind() != Kind::EntityDecl) {
        let mut at = node.properties();
        while let Some(a) = at {
            let named = match &view.walk {
                Walk::Attributes(Some(name)) => a.name() == *name,
                Walk::Attributes(None) | Walk::Node | Walk::Elements(_) | Walk::Children => true,
            };
            if named && view.matches_ns(a) {
                return Ok(false);
            }
            at = a.next();
        }
    }
    let Some(node) = view.first(base) else {
        return Ok(true);
    };
    if matches!(view.walk, Walk::Attributes(_)) {
        return Ok(true);
    }
    if node.kind() == Kind::Attribute {
        return Ok(false);
    }
    let mut use_iter = false;
    let mut at = if view.walk == Walk::Children {
        Some(node)
    } else if view.walk == Walk::Node
        || node.children().is_none()
        || node.parent().is_none()
        || node.children().and_then(Node::next).is_some()
        || node.children().and_then(Node::children).is_some()
        || node.parent().and_then(Node::children) == node.parent().and_then(Node::last)
    {
        node.children()
    } else {
        use_iter = true;
        view.reset(base)
    };
    while let Some(n) = at {
        let taken = if n.children().is_some() || n.prev().is_some() || n.next().is_some() {
            n.kind() != Kind::Text
        } else if n.kind() == Kind::Text {
            if n.content().is_some_and(|c| !c.is_empty()) {
                return Ok(false);
            }
            false
        } else {
            true
        };
        if taken && (n.kind() != Kind::Element || view.matches_ns(n)) && n.has_name() {
            return Ok(false);
        }
        at = if use_iter {
            view.fetch(n.next())
        } else {
            n.next()
        };
    }
    Ok(true)
}

/// `SimpleXMLElement::__debugInfo`'s table, `sxe_get_prop_hash(obj, 1)`.
pub(crate) fn debug_table(req: &mut Request, object: PhpObject) -> Result<Owner<PhpArray>, Thrown> {
    let rows = prop_hash(req, object, true)?;
    table(req, rows)
}

/// `sxe_get_properties`: the table rebuilt and kept on the object, what
/// it held before released first, as `zend_hash_clean` empties php's.
fn properties(req: &mut Request, object: PhpObject) -> Result<PhpArray, Thrown> {
    if let Some(old) = sxe_mut(req, object).and_then(|s| s.properties.take()) {
        old.give_up(req);
    }
    let rows = prop_hash(req, object, false)?;
    let built = table(req, rows)?;
    let view = *built;
    // The payload keeps the table's owner.
    match sxe_mut(req, object) {
        Some(s) => s.properties = Some(built),
        None => built.give_up(req),
    }
    Ok(view)
}

// ---- casts and counts

/// `sxe_object_cast_ex`'s contents: the text of the walk's first node, or
/// of the node itself (a comment's or a PI's own content where it has no
/// children); `None` where there is none.
pub(crate) fn cast_text(req: &mut Request, object: PhpObject) -> Result<Option<Vec<u8>>, Thrown> {
    let view = View::of(req, object);
    if view.walk != Walk::Node {
        let base = view.node(req)?;
        return Ok(view.first(base).and_then(Node::list_string));
    }
    let Some(node) = view.node else {
        return Ok(None);
    };
    if node.children().is_some() {
        return Ok(node.list_string());
    }
    Ok(match node.kind() {
        Kind::Comment | Kind::Pi => node.content(),
        _ => None,
    })
}

/// `sxe_object_cast_ex`.
fn cast(req: &mut Request, object: PhpObject, to: TypeTag) -> Result<Option<Owner<Value>>, Thrown> {
    if to == TypeTag::Bool {
        let view = View::of(req, object);
        let first = match view.walk {
            Walk::Node => None,
            Walk::Elements(_) | Walk::Children | Walk::Attributes(_) => {
                let base = view.node(req)?;
                view.first(base)
            }
        };
        let truth = first.is_some() || !prop_is_empty(req, object)?;
        return Ok(Some(Owner::bool(truth)));
    }
    let text = cast_text(req, object)?;
    let value = match text {
        Some(t) => string(req, &t)?,
        None => Owner::null(),
    };
    let converted = match to {
        TypeTag::String => to_string(req, &value).map(Owner::into_value),
        TypeTag::Int => to_long(req, &value).map(Owner::int),
        TypeTag::Float => to_double(req, &value).map(Owner::float),
        TypeTag::Number => to_number(req, &value).map(|n| Owner::take(req, n)),
        _ => {
            value.give_up(req);
            return Ok(None);
        }
    };
    value.give_up(req);
    converted.map(Some)
}

/// `php_sxe_count_elements_helper`.
pub(crate) fn count(req: &mut Request, object: PhpObject) -> Result<i64, Thrown> {
    let view = View::of(req, object);
    let base = view.node(req)?;
    let mut at = view.reset(base);
    let mut n = 0;
    while let Some(node) = at {
        n += 1;
        at = view.fetch(node.next());
    }
    Ok(n)
}

/// Whether the class overrides `method` with a user's, which php's
/// `php_sxe_find_fptr_count` keeps for `count`.
fn overridden(req: &mut Request, object: PhpObject, lowered: &[u8]) -> bool {
    Std.get_method(req, object, lowered)
        .ok()
        .flatten()
        .and_then(|method| req.symbols().function_decl(method))
        .is_some_and(|decl| !decl.file.0.is_empty())
}

// ---- the object's own iteration, `iter.data`

/// `php_sxe_reset_iterator`: `iter.data` let go of and set to the walk's
/// first node.
pub(crate) fn rewind(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    if let Some(old) = sxe_mut(req, object).and_then(|s| s.current.take()) {
        old.give_up(req);
    }
    let view = View::of(req, object);
    let base = view.node(req)?;
    if let Some(first) = view.reset(base) {
        let made = wrap(req, object, first, Walk::Node, view.ns.clone())?;
        set_current(req, object, made);
    }
    Ok(())
}

/// `php_sxe_move_forward_iterator`.
pub(crate) fn move_forward(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    let Some(current) = sxe_mut(req, object).and_then(|s| s.current.take()) else {
        return Ok(());
    };
    let node = View::of(req, *current).node(req);
    current.give_up(req);
    let node = node?;
    let view = View::of(req, object);
    if let Some(next) = view.fetch(node.next()) {
        let made = wrap(req, object, next, Walk::Node, view.ns.clone())?;
        set_current(req, object, made);
    }
    Ok(())
}

fn set_current(req: &mut Request, object: PhpObject, made: Owner<PhpObject>) {
    match sxe_mut(req, object) {
        Some(s) => s.current = Some(made),
        None => made.give_up(req),
    }
}

/// `iter.data`, a view of it.
pub(crate) fn current(req: &mut Request, object: PhpObject) -> Option<Value> {
    Some(sxe(req, object)?.current.as_deref()?.as_value())
}

/// The name of the node `iter.data` stands for, `php_sxe_iterator_current_key`.
pub(crate) fn current_name(req: &Request, object: PhpObject) -> Option<Vec<u8>> {
    let current = *sxe(req, object)?.current.as_deref()?;
    sxe(req, current)?.obj.bound().map(Node::name)
}

/// `php_sxe_iterator`: a loop over the object walks the object's own
/// `iter.data`, as php's iterator reads `iterator->sxe->iter.data`.
struct Loop;

impl ClassIterator for Loop {
    fn rewind(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        rewind(req, object)
    }

    fn valid(&mut self, req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
        Ok(sxe(req, object).is_some_and(|s| s.current.is_some()))
    }

    fn current(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        // The iterator's `current` hands its caller an owner.
        let value = current(req, object).unwrap_or_else(Value::null);
        Ok(Owner::take(req, value))
    }

    fn key(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        match current_name(req, object) {
            Some(name) => string(req, &name),
            None => Ok(Owner::null()),
        }
    }

    fn move_forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        move_forward(req, object)
    }
}

// ---- clone and compare

/// `sxe_object_clone`: a root element's clone holds a copy of the whole
/// document, any other node's a copy of the node in the same document,
/// standing apart from it.
fn clone(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let view = View::of(req, object);
    let shared = sxe(req, object).and_then(|s| s.obj.shared_doc());
    let doc = sxe(req, object).and_then(|s| s.obj.document_node());
    let root = view
        .node
        .and_then(Node::parent)
        .is_some_and(|p| p.kind().is_document());
    let made = new_object(req, object.class())?;
    let Some(mut obj) = libxml_half(req, *made) else {
        return Ok(made);
    };
    let doc = if root {
        let copy = doc.and_then(|d| d.copy_doc(true));
        if let Some(c) = copy {
            obj.adopt_document(req, c);
        }
        copy
    } else {
        if let (Some(shared), Some(d)) = (shared, doc) {
            obj.share(req, Some(shared), d);
        }
        doc
    };
    if let Some(s) = sxe_mut(req, *made) {
        s.walk = view.walk;
        s.ns = view.ns;
    }
    let node = view.node.and_then(|n| {
        if root {
            doc.and_then(Node::root)
        } else {
            n.copy(doc, 1)
        }
    });
    if let Some(n) = node {
        obj.hold_node(req, n);
    }
    Ok(made)
}

/// The table `SimpleXMLElement` and its subclasses are reached through,
/// `sxe_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

impl ObjectHandlers for Handlers {
    /// `php_sxe_iterator`: the iterator and its `sxe`.
    fn iterator_size(&self) -> usize {
        96
    }

    /// `sxe_object_free_storage`: `zend_object_std_dtor` first, then what
    /// the iterator and the property table held.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        Std.free_obj(req, object);
        let (current, properties, made) = match sxe_mut(req, object) {
            Some(s) => (s.current.take(), s.properties.take(), s.made.take()),
            None => (None, None, None),
        };
        for held in [current, made].into_iter().flatten() {
            held.give_up(req);
        }
        if let Some(p) = properties {
            p.give_up(req);
        }
        // `sxe_object_free_iterxpath`, then
        // `php_libxml_node_decrement_resource`.
        if let Some(s) = sxe_mut(req, object) {
            s.xpath = None;
        }
        if let Some(mut obj) = libxml_half(req, object) {
            obj.release(req);
        }
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone)
    }

    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        let name = c_name(name);
        if read == PropertyRead::Write {
            // `sxe_property_get_adr`: a write through a name no element
            // has makes the element first.
            let view = View::of(req, object);
            let node = view.node(req)?;
            if !view.has_element(node, &name) {
                let made =
                    prop_dim_write(req, object, &Member::Name(name), None, Access::Elements)?;
                let Some(n) = made else {
                    return Ok(Owner::null());
                };
                let wrapped = wrap(req, object, n, Walk::Node, view.ns)?;
                let value = Owner::take(req, wrapped.as_value());
                let old = if let Some(s) = sxe_mut(req, object) {
                    s.made.replace(wrapped)
                } else {
                    wrapped.give_up(req);
                    None
                };
                if let Some(old) = old {
                    old.give_up(req);
                }
                return Ok(value);
            }
        }
        let fetch = match read {
            PropertyRead::Silent => Fetch::Silent,
            PropertyRead::Write => Fetch::Write,
            PropertyRead::Warning | PropertyRead::Unset => Fetch::Read,
        };
        prop_dim_read(req, object, Member::Name(name), Access::Elements, fetch)
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let member = Member::Name(c_name(name.as_bytes()));
        name.give_up(req);
        let written = prop_dim_write(req, object, &member, Some(&value), Access::Elements);
        value.give_up(req);
        written.map(|_| ())
    }

    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        let member = Member::of(req, offset, false)?;
        let fetch = match read {
            DimensionRead::Silent => Fetch::Silent,
            DimensionRead::Write | DimensionRead::ReadWrite => Fetch::Write,
            DimensionRead::Warning | DimensionRead::Unset => Fetch::Read,
        };
        prop_dim_read(req, object, member, Access::Attributes, fetch)
    }

    fn write_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let written = Member::of(req, offset, true).and_then(|member| {
            prop_dim_write(req, object, &member, Some(&value), Access::Attributes)
        });
        value.give_up(req);
        written.map(|_| ())
    }

    /// `sxe_property_get_adr`'s verdict: a name some element has is the
    /// handlers', and a name none has a place the object keeps apart
    /// (`&sxe->tmp`), which the write fetch lays down: a compound write
    /// or a step over it changes that value and writes nothing back.
    ///
    /// ```php
    /// $x = simplexml_load_string('<r><a>1</a></r>');
    /// $x->a .= 'x'; $x->n .= 'y'; $x->m++;
    /// $x->p->q = 'v'; $x->l[] = 'w';
    /// echo $x->asXML();
    /// ```
    /// ```expect
    /// <?xml version="1.0"?>
    /// <r><a>1x</a><n/><m/><p><q>v</q></p><l>w</l></r>
    /// ```
    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        let view = View::of(req, object);
        Ok(match view.node {
            Some(node) if !view.has_element(node, &c_name(name)) => PropertyPlace::Apart,
            Some(_) | None => PropertyPlace::Handlers,
        })
    }

    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        prop_dim_exists(
            req,
            object,
            &Member::Name(c_name(name)),
            test,
            Access::Elements,
        )
    }

    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        prop_dim_delete(req, object, &Member::Name(c_name(name)), Access::Elements)
    }

    fn has_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
        not_empty: bool,
    ) -> Result<bool, Thrown> {
        let member = Member::of(req, Some(offset), false)?;
        let test = if not_empty {
            PropertyTest::NotEmpty
        } else {
            PropertyTest::Isset
        };
        prop_dim_exists(req, object, &member, test, Access::Attributes)
    }

    fn unset_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        let member = Member::of(req, Some(offset), false)?;
        prop_dim_delete(req, object, &member, Access::Attributes)
    }

    /// `sxe_get_properties`: the table rebuilt and kept on the object.
    fn get_properties(&self, req: &mut Request, object: PhpObject) -> Result<walk::Table, Thrown> {
        properties(req, object).map(walk::Table::Array)
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(p) = sxe(req, object).and_then(|s| s.properties.as_deref()) {
            visit(Value::array(*p));
        }
        GcTable::Properties
    }

    fn cast_object(
        &self,
        req: &mut Request,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        if to == TypeTag::String
            && let Some(text) = Std.cast_object(req, object, TypeTag::String)?
        {
            return Ok(Some(text));
        }
        cast(req, object, to)
    }

    fn count_elements(&self, req: &mut Request, object: PhpObject) -> Result<Option<i64>, Thrown> {
        if overridden(req, object, b"count") {
            let answered = call_method(req, &object, b"count", &[])?;
            let n = to_long(req, &answered);
            answered.give_up(req);
            return n.map(Some);
        }
        count(req, object).map(Some)
    }

    /// `sxe_objects_compare`: two elements are equal where they are the
    /// same node, two documents where they are the same document, and two
    /// objects nothing initialised never.
    ///
    /// ```php
    /// class S extends SimpleXMLElement { function __construct() {} }
    /// $a = new S; $b = new S; $x = simplexml_load_string('<r><c/></r>');
    /// var_dump($a == $a, $a == $b, $a == $x, $x->c == $x->c, $x == $x->c);
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// ```
    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<std::cmp::Ordering>, Thrown> {
        let (l, r) = (left.deref(), right.deref());
        let pair = match (l.as_object(), r.as_object()) {
            (Some(a), Some(b)) => sxe(req, a).zip(sxe(req, b)).map(|(x, y)| {
                let nodes = (x.obj.bound(), y.obj.bound());
                let docs = (x.obj.document_node(), y.obj.document_node());
                (nodes, docs)
            }),
            _ => None,
        };
        let Some((nodes, docs)) = pair else {
            return Std.compare(req, left, right);
        };
        Ok(match nodes {
            (Some(a), Some(b)) => (a == b).then_some(std::cmp::Ordering::Equal),
            // Two objects nothing initialised share no document, so they
            // are not equal (php 8.5.12).
            (None, None) => {
                (docs.0.is_some() && docs.0 == docs.1).then_some(std::cmp::Ordering::Equal)
            }
            _ => None,
        })
    }

    fn get_iterator(
        &self,
        req: &mut Request,
        _: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        if by_ref {
            return Err(throw_error(
                req,
                "Error",
                "An iterator cannot be used with foreach by reference",
            ));
        }
        Ok(Some(Box::new(Loop)))
    }
}
