//! `php_schema.c`: the XML Schema of a WSDL read into the description's
//! types, elements, groups and encoders (`load_schema` and every
//! `schema_*` reader), and pass two, which resolves the `ref`s, the
//! group references and the attribute groups once every schema is read
//! (`schema_pass2`). Each reader is php's, in its order: what a table
//! adds, updates or pushes is what `__getTypes()` and the encoders walk.

use std::borrow::Cow;

use php_ext::coerce::atoi;
use php_ext::request::Request;
use php_ext_libxml::ffi::{Library, Node};

use crate::model::{
    AttrUse, Attribute, Content, ContentKind, ContentModel, EncRef, EncodeType, Encoder,
    ExtraAttribute, Form, RestrictionChar, RestrictionInt, Sdl, ToXml, ToZval, Type, TypeId,
    TypeKind, XSD_1999_NAMESPACE, XSD_DRAFT_2000_NAMESPACE, XSD_NAMESPACE, get_conversion, ty,
};
use crate::sdl::wsdl::{Ctx, parse_with_credentials};
use crate::sdl::{get_encoder, nscat};
use crate::table::{Key, Table};
use crate::xml::{
    attr_content, attr_is_equal, attribute, get_node, href, node_is_equal_one_of, parse_namespace,
};
use crate::{Error, Res, soap_error};

/// Bytes as the text php prints them with `%s`.
pub(crate) fn lossy(bytes: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

/// `node_is_equal_xsd`: the node named `name` in one of XML Schema's
/// three namespaces.
fn node_is_equal_xsd(node: Node, name: &[u8]) -> bool {
    node_is_equal_one_of(
        node,
        name,
        &[XSD_NAMESPACE, XSD_DRAFT_2000_NAMESPACE, XSD_1999_NAMESPACE],
    )
}

/// `soap_error1(E_ERROR, "Parsing Schema: unexpected <%s> in %s", ...)`.
fn unexpected(node: Node, within: &str) -> Error {
    soap_error(format_args!(
        "Parsing Schema: unexpected <{}> in {within}",
        lossy(&node.name())
    ))
}

/// `soap_error0(E_ERROR, "Parsing Schema: ...")`.
fn schema_error(text: &str) -> Error {
    soap_error(format_args!("Parsing Schema: {text}"))
}

/// The first child, past an `<annotation>`.
fn after_annotation(node: Node) -> Option<Node> {
    let trav = node.children();
    match trav {
        Some(t) if node_is_equal_xsd(t, b"annotation") => t.next(),
        _ => trav,
    }
}

/// `ns:name` joined.
fn join(ns: &[u8], name: &[u8]) -> Vec<u8> {
    nscat(Some(ns), name)
}

/// `{ns}type`.
fn clark(ns: &[u8], type_: &[u8]) -> Vec<u8> {
    let mut c = Vec::with_capacity(ns.len() + type_.len() + 2);
    c.push(b'{');
    c.extend_from_slice(ns);
    c.push(b'}');
    c.extend_from_slice(type_);
    c
}

/// An `encode` of the description's own, `sdl_guess_convert_*` by its
/// SDL type.
fn sdl_encoder(
    ns: Option<Vec<u8>>,
    type_str: Option<Vec<u8>>,
    sdl_type: Option<TypeId>,
) -> Encoder {
    let clark_notation = ns
        .as_deref()
        .map(|n| clark(n, type_str.as_deref().unwrap_or_default()));
    Encoder {
        details: EncodeType {
            type_: 0,
            type_str,
            ns,
            clark_notation,
            sdl_type,
        },
        to_zval: ToZval::SdlGuess,
        to_xml: ToXml::SdlGuess,
    }
}

/// `get_encoder` on a description still being read: where it falls back
/// from a SOAP encoding namespace to XML Schema's, the copy php makes is
/// added to the description's encoders under the name asked for
/// (`zend_hash_str_update_ptr`), as the cache file then holds it.
pub(crate) fn get_encoder_mut(sdl: &mut Sdl, ns: Option<&[u8]>, type_: &[u8]) -> Option<EncRef> {
    let found = get_encoder(Some(sdl), ns, type_)?;
    let EncRef::DefaultIn(at, _) = found else {
        return Some(found);
    };
    let row = crate::model::builtin(at)?;
    let mut enc = row.encoder();
    enc.details.ns = ns.map(<[u8]>::to_vec);
    let id = sdl.new_encoder(enc);
    sdl.encoders
        .get_or_insert_with(Table::new)
        .update(Key::Str(nscat(ns, type_)), id);
    Some(EncRef::Sdl(id))
}

/// `get_encoder_from_prefix` on a description still being read, through
/// [`get_encoder_mut`].
pub(crate) fn get_encoder_from_prefix_mut(
    sdl: &mut Sdl,
    node: Node,
    type_: &[u8],
) -> Option<EncRef> {
    let (cptype, prefix) = parse_namespace(type_);
    match node.search_ns(prefix) {
        Some(ns) => {
            let h = href(ns);
            get_encoder_mut(sdl, Some(&h), cptype)
                .or_else(|| crate::sdl::get_encoder_ex(Some(sdl), cptype))
        }
        None => crate::sdl::get_encoder_ex(Some(sdl), type_),
    }
}

/// `create_encoder`: the encoder named `ns:type`, reset in place where
/// the description has one, else made and added.
pub(crate) fn create_encoder(
    sdl: &mut Sdl,
    cur_type: Option<TypeId>,
    ns: &[u8],
    type_: &[u8],
) -> EncRef {
    let key = Key::Str(join(ns, type_));
    let enc = sdl_encoder(Some(ns.to_vec()), Some(type_.to_vec()), cur_type);
    let existing = sdl
        .encoders
        .get_or_insert_with(Table::new)
        .get(&key)
        .copied();
    if let Some(id) = existing {
        if let Some(slot) = sdl.encoder_mut(id) {
            *slot = enc;
        }
        return EncRef::Sdl(id);
    }
    let id = sdl.new_encoder(enc);
    if let Some(encoders) = sdl.encoders.as_mut() {
        encoders.update(key, id);
    }
    EncRef::Sdl(id)
}

/// `get_create_encoder`.
pub(crate) fn get_create_encoder(
    sdl: &mut Sdl,
    cur_type: Option<TypeId>,
    ns: &[u8],
    type_: &[u8],
) -> EncRef {
    match get_encoder_mut(sdl, Some(ns), type_) {
        Some(found) => found,
        None => create_encoder(sdl, cur_type, ns, type_),
    }
}

/// `schema_load_file`: an imported, included or redefined schema read
/// once per location and loaded, its `targetNamespace` held to the one
/// expected.
fn schema_load_file(
    req: &mut Request,
    ctx: &mut Ctx,
    ns: Option<Node>,
    location: Option<Vec<u8>>,
    tns: Node,
    import: bool,
) -> Res<()> {
    let Some(location) = location else {
        return Ok(());
    };
    if ctx.docs.contains(&Key::Str(location.clone())) {
        return Ok(());
    }
    let (doc, pending) = parse_with_credentials(req, ctx, &location)?;
    let loc = lossy(&location).into_owned();
    let Some(doc) = doc else {
        return Err(Error::soap_after(
            format_args!("Parsing Schema: can't import schema from '{loc}'"),
            pending,
        ));
    };
    let Some(schema) = get_node(doc.children(), b"schema", None) else {
        doc.free_document();
        return Err(schema_error(&format!("can't import schema from '{loc}'")));
    };
    let new_tns = attribute(schema, b"targetNamespace");
    if import {
        if let Some(ns) = ns
            && new_tns.is_none_or(|t| attr_content(ns) != attr_content(t))
        {
            let expected = lossy(&attr_content(ns)).into_owned();
            let text = match new_tns {
                None => format!(
                    "can't import schema from '{loc}', missing 'targetNamespace', expected '{expected}'"
                ),
                Some(t) => format!(
                    "can't import schema from '{loc}', unexpected 'targetNamespace'='{}', expected '{expected}'",
                    lossy(&attr_content(t))
                ),
            };
            doc.free_document();
            return Err(schema_error(&text));
        }
        if ns.is_none()
            && let Some(t) = new_tns
        {
            let text = format!(
                "can't import schema from '{loc}', unexpected 'targetNamespace'='{}', expected no 'targetNamespace'",
                lossy(&attr_content(t))
            );
            doc.free_document();
            return Err(schema_error(&text));
        }
    } else {
        match new_tns {
            None => {
                schema.set_prop(b"targetNamespace", &attr_content(tns));
            }
            Some(t) if attr_content(tns) != attr_content(t) => {
                doc.free_document();
                return Err(schema_error(&format!(
                    "can't include schema from '{loc}', different 'targetNamespace'"
                )));
            }
            Some(_) => {}
        }
    }
    ctx.docs.add(Key::Str(location), doc);
    load_schema(req, ctx, schema)
}

/// `schema_location_construct_uri`: an attribute's value resolved
/// against the base of its element, or its document's URL.
pub(crate) fn schema_location_construct_uri(attr: Node) -> Option<Vec<u8>> {
    let value = attr_content(attr);
    match attr.parent().and_then(Node::base) {
        None => {
            let url = attr.doc().and_then(Node::doc_url);
            Library::build_uri(&value, url.as_deref())
        }
        Some(base) => Library::build_uri(&value, Some(&base)),
    }
}

/// `load_schema`: one `<schema>`, its includes, redefines and imports
/// first, then its declarations in order.
pub(crate) fn load_schema(req: &mut Request, ctx: &mut Ctx, schema: Node) -> Res<()> {
    ctx.sdl.types.get_or_insert_with(Table::new);
    ctx.attributes.get_or_insert_with(Table::new);
    ctx.attribute_groups.get_or_insert_with(Table::new);

    let tns = if let Some(t) = attribute(schema, b"targetNamespace") {
        t
    } else {
        let made = schema.set_prop(b"targetNamespace", b"");
        schema.new_ns(b"", None);
        let Some(t) = made else {
            return Err(schema_error("can't set 'targetNamespace'"));
        };
        t
    };

    let mut trav = schema.children();
    while let Some(t) = trav {
        if node_is_equal_xsd(t, b"include") {
            let Some(location) = attribute(t, b"schemaLocation") else {
                return Err(schema_error("include has no 'schemaLocation' attribute"));
            };
            let uri = schema_location_construct_uri(location);
            schema_load_file(req, ctx, None, uri, tns, false)?;
        } else if node_is_equal_xsd(t, b"redefine") {
            let Some(location) = attribute(t, b"schemaLocation") else {
                return Err(schema_error("redefine has no 'schemaLocation' attribute"));
            };
            let uri = schema_location_construct_uri(location);
            schema_load_file(req, ctx, None, uri, tns, false)?;
        } else if node_is_equal_xsd(t, b"import") {
            let ns = attribute(t, b"namespace");
            let location = attribute(t, b"schemaLocation");
            if let Some(n) = ns
                && attr_content(n) == attr_content(tns)
            {
                return Err(match location {
                    Some(l) => schema_error(&format!(
                        "can't import schema from '{}', namespace must not match the enclosing schema 'targetNamespace'",
                        lossy(&attr_content(l))
                    )),
                    None => schema_error(
                        "can't import schema. Namespace must not match the enclosing schema 'targetNamespace'",
                    ),
                });
            }
            let uri = location.and_then(schema_location_construct_uri);
            schema_load_file(req, ctx, ns, uri, tns, true)?;
        } else if !node_is_equal_xsd(t, b"annotation") {
            break;
        }
        trav = t.next();
    }

    while let Some(t) = trav {
        if node_is_equal_xsd(t, b"simpleType") {
            schema_simple_type(ctx, tns, t, None)?;
        } else if node_is_equal_xsd(t, b"complexType") {
            schema_complex_type(ctx, tns, t, None)?;
        } else if node_is_equal_xsd(t, b"group") {
            schema_group(ctx, tns, t, None, None)?;
        } else if node_is_equal_xsd(t, b"attributeGroup") {
            schema_attribute_group(ctx, tns, t, None)?;
        } else if node_is_equal_xsd(t, b"element") {
            schema_element(ctx, tns, t, None, None)?;
        } else if node_is_equal_xsd(t, b"attribute") {
            schema_attribute(ctx, tns, t, None)?;
        } else if !node_is_equal_xsd(t, b"notation") && !node_is_equal_xsd(t, b"annotation") {
            return Err(unexpected(t, "schema"));
        }
        trav = t.next();
    }
    Ok(())
}

/// The type `id`, which every reader made before it handed it on.
fn ty_mut(ctx: &mut Ctx, id: TypeId) -> Res<&mut Type> {
    ctx.sdl
        .ty_mut(id)
        .ok_or_else(|| schema_error("internal error: a type out of range"))
}

/// `zend_hash_next_index_insert_ptr(sdl->types, type)`.
fn push_type(sdl: &mut Sdl, t: Type) -> TypeId {
    let id = sdl.new_type(t);
    sdl.types.get_or_insert_with(Table::new).push(id);
    id
}

/// `anonymous` and the number of types the description has so far.
fn anonymous_name(sdl: &Sdl) -> Vec<u8> {
    let n = sdl.types.as_ref().map_or(0, Table::len);
    format!("anonymous{n}").into_bytes()
}

/// The anonymous type of an element or a restriction (`cur_type` set):
/// a new type in the description's types, named after the one given
/// where it has no name of its own, and an encoder of its own on the
/// one given, pushed to the encoders. The new type's id.
fn anonymous_type(
    ctx: &mut Ctx,
    kind: TypeKind,
    name: Option<Node>,
    ns: Node,
    cur: TypeId,
) -> Res<TypeId> {
    let (name, namens) = if let Some(n) = name {
        (Some(attr_content(n)), Some(attr_content(ns)))
    } else {
        let c = ty_mut(ctx, cur)?;
        (c.name.clone(), c.namens.clone())
    };
    let ptr = push_type(
        &mut ctx.sdl,
        Type {
            kind,
            name: name.clone(),
            namens: namens.clone(),
            ..Type::default()
        },
    );
    let eid = ctx.sdl.new_encoder(sdl_encoder(namens, name, Some(ptr)));
    ctx.sdl.encoders.get_or_insert_with(Table::new).push(eid);
    ty_mut(ctx, cur)?.encode = Some(EncRef::Sdl(eid));
    Ok(ptr)
}

/// `schema_simpleType`.
fn schema_simple_type(
    ctx: &mut Ctx,
    tns: Node,
    simple_type: Node,
    cur_type: Option<TypeId>,
) -> Res<()> {
    let ns = attribute(simple_type, b"targetNamespace").unwrap_or(tns);
    let name = attribute(simple_type, b"name");
    let cur = if let Some(cur) = cur_type {
        anonymous_type(ctx, TypeKind::Simple, name, ns, cur)?
    } else if let Some(name) = name {
        let (n, nsv) = (attr_content(name), attr_content(ns));
        let ptr = push_type(
            &mut ctx.sdl,
            Type {
                kind: TypeKind::Simple,
                name: Some(n.clone()),
                namens: Some(nsv.clone()),
                ..Type::default()
            },
        );
        create_encoder(&mut ctx.sdl, Some(ptr), &nsv, &n);
        ptr
    } else {
        return Err(schema_error("simpleType has no 'name' attribute"));
    };

    let Some(t) = after_annotation(simple_type) else {
        return Err(schema_error(
            "expected <restriction>, <list> or <union> in simpleType",
        ));
    };
    if node_is_equal_xsd(t, b"restriction") {
        schema_restriction_simple_content(ctx, tns, t, cur, true)?;
    } else if node_is_equal_xsd(t, b"list") {
        ty_mut(ctx, cur)?.kind = TypeKind::List;
        schema_list(ctx, tns, t, cur)?;
    } else if node_is_equal_xsd(t, b"union") {
        ty_mut(ctx, cur)?.kind = TypeKind::Union;
        schema_union(ctx, tns, t, cur)?;
    } else {
        return Err(unexpected(t, "simpleType"));
    }
    match t.next() {
        Some(n) => Err(unexpected(n, "simpleType")),
        None => Ok(()),
    }
}

/// A member of a list or a union named by `prefix:type`: a type of the
/// referenced name with its encoder, pushed to the elements of `cur`.
fn member_type(ctx: &mut Ctx, at: Node, qname: &[u8], cur: TypeId) -> Res<()> {
    let (type_, prefix) = parse_namespace(qname);
    if let Some(nsptr) = at.search_ns(prefix) {
        let h = href(nsptr);
        let id = ctx.sdl.new_type(Type {
            name: Some(type_.to_vec()),
            namens: Some(h.clone()),
            ..Type::default()
        });
        let enc = get_create_encoder(&mut ctx.sdl, Some(id), &h, type_);
        ty_mut(ctx, id)?.encode = Some(enc);
        ty_mut(ctx, cur)?
            .elements
            .get_or_insert_with(Table::new)
            .push(id);
    }
    Ok(())
}

/// An anonymous `<simpleType>` of a list or a union: a type named
/// `anonymousN` in the schema's namespace, pushed to the elements of
/// `cur` and read.
fn anonymous_member(ctx: &mut Ctx, tns: Node, simple: Node, cur: TypeId) -> Res<()> {
    let name = anonymous_name(&ctx.sdl);
    let id = ctx.sdl.new_type(Type {
        name: Some(name),
        namens: Some(attr_content(tns)),
        ..Type::default()
    });
    ty_mut(ctx, cur)?
        .elements
        .get_or_insert_with(Table::new)
        .push(id);
    schema_simple_type(ctx, tns, simple, Some(id))
}

/// `schema_list`.
fn schema_list(ctx: &mut Ctx, tns: Node, list: Node, cur: TypeId) -> Res<()> {
    let item_type = attribute(list, b"itemType");
    if let Some(item) = item_type {
        member_type(ctx, list, &attr_content(item), cur)?;
    }
    let mut trav = after_annotation(list);
    if let Some(t) = trav
        && node_is_equal_xsd(t, b"simpleType")
    {
        if item_type.is_some() {
            return Err(schema_error(
                "element has both 'itemType' attribute and subtype",
            ));
        }
        anonymous_member(ctx, tns, t, cur)?;
        trav = t.next();
    }
    match trav {
        Some(t) => Err(unexpected(t, "list")),
        None => Ok(()),
    }
}

/// `whiteSpace_collapse`: tabs, newlines and carriage returns made
/// spaces, runs of spaces made one, and the ends trimmed.
fn white_space_collapse(s: &[u8]) -> Vec<u8> {
    let replaced = s.iter().map(|&c| {
        if matches!(c, 0x9 | 0xa | 0xd) {
            b' '
        } else {
            c
        }
    });
    let mut out = Vec::with_capacity(s.len());
    let mut old = 0u8;
    for c in replaced.skip_while(|c| *c == b' ') {
        if c != b' ' || old != b' ' {
            out.push(c);
        }
        old = c;
    }
    if old == b' ' {
        out.pop();
    }
    out
}

/// `schema_union`.
fn schema_union(ctx: &mut Ctx, tns: Node, union: Node, cur: TypeId) -> Res<()> {
    if let Some(members) = attribute(union, b"memberTypes") {
        let collapsed = white_space_collapse(&attr_content(members));
        for member in collapsed.split(|c| *c == b' ').filter(|m| !m.is_empty()) {
            member_type(ctx, union, member, cur)?;
        }
    }
    let mut trav = after_annotation(union);
    while let Some(t) = trav {
        if node_is_equal_xsd(t, b"simpleType") {
            anonymous_member(ctx, tns, t, cur)?;
        } else {
            return Err(unexpected(t, "union"));
        }
        trav = t.next();
    }
    Ok(())
}

/// `schema_simpleContent`.
fn schema_simple_content(ctx: &mut Ctx, tns: Node, node: Node, cur: TypeId) -> Res<()> {
    let Some(t) = after_annotation(node) else {
        return Err(schema_error(
            "expected <restriction> or <extension> in simpleContent",
        ));
    };
    if node_is_equal_xsd(t, b"restriction") {
        ty_mut(ctx, cur)?.kind = TypeKind::Restriction;
        schema_restriction_simple_content(ctx, tns, t, cur, false)?;
    } else if node_is_equal_xsd(t, b"extension") {
        ty_mut(ctx, cur)?.kind = TypeKind::Extension;
        schema_extension_simple_content(ctx, tns, t, cur)?;
    } else {
        return Err(unexpected(t, "simpleContent"));
    }
    match t.next() {
        Some(n) => Err(unexpected(n, "simpleContent")),
        None => Ok(()),
    }
}

/// A `base` attribute's type, its encoder put on `cur` where its prefix
/// is in scope.
fn base_encoder(ctx: &mut Ctx, node: Node, base: Node, cur: TypeId) -> Res<()> {
    let value = attr_content(base);
    let (type_, prefix) = parse_namespace(&value);
    if let Some(nsptr) = node.search_ns(prefix) {
        let enc = get_create_encoder(&mut ctx.sdl, Some(cur), &href(nsptr), type_);
        ty_mut(ctx, cur)?.encode = Some(enc);
    }
    Ok(())
}

/// The `(attribute | attributeGroup)*, anyAttribute?` tail of a
/// restriction, an extension or a complex type: the node after it.
fn attribute_tail(
    ctx: &mut Ctx,
    tns: Node,
    mut trav: Option<Node>,
    cur: TypeId,
    within: &str,
) -> Res<Option<Node>> {
    while let Some(t) = trav {
        if node_is_equal_xsd(t, b"attribute") {
            schema_attribute(ctx, tns, t, Some(cur))?;
        } else if node_is_equal_xsd(t, b"attributeGroup") {
            schema_attribute_group(ctx, tns, t, Some(cur))?;
        } else if node_is_equal_xsd(t, b"anyAttribute") {
            return Ok(t.next());
        } else {
            return Err(unexpected(t, within));
        }
        trav = t.next();
    }
    Ok(None)
}

/// `schema_restriction_simpleContent`, for a `<simpleType>`'s
/// restriction (`simple_type`) or a `<simpleContent>`'s.
fn schema_restriction_simple_content(
    ctx: &mut Ctx,
    tns: Node,
    rest: Node,
    cur: TypeId,
    simple_type: bool,
) -> Res<()> {
    match attribute(rest, b"base") {
        Some(base) => base_encoder(ctx, rest, base, cur)?,
        None if !simple_type => {
            return Err(schema_error("restriction has no 'base' attribute"));
        }
        None => {}
    }
    ty_mut(ctx, cur)?
        .restrictions
        .get_or_insert_with(Box::default);

    let mut trav = after_annotation(rest);
    if let Some(t) = trav
        && node_is_equal_xsd(t, b"simpleType")
    {
        schema_simple_type(ctx, tns, t, Some(cur))?;
        trav = t.next();
    }
    while let Some(t) = trav {
        let int_facet = [
            &b"minExclusive"[..],
            b"minInclusive",
            b"maxExclusive",
            b"maxInclusive",
            b"totalDigits",
            b"fractionDigits",
            b"length",
            b"minLength",
            b"maxLength",
        ]
        .into_iter()
        .position(|f| node_is_equal_xsd(t, f));
        if let Some(at) = int_facet {
            let value = schema_restriction_var_int(t)?;
            let r = ty_mut(ctx, cur)?
                .restrictions
                .get_or_insert_with(Box::default);
            let slot = match at {
                0 => &mut r.min_exclusive,
                1 => &mut r.min_inclusive,
                2 => &mut r.max_exclusive,
                3 => &mut r.max_inclusive,
                4 => &mut r.total_digits,
                5 => &mut r.fraction_digits,
                6 => &mut r.length,
                7 => &mut r.min_length,
                _ => &mut r.max_length,
            };
            *slot = Some(value);
        } else if node_is_equal_xsd(t, b"whiteSpace") {
            let value = schema_restriction_var_char(t)?;
            ty_mut(ctx, cur)?
                .restrictions
                .get_or_insert_with(Box::default)
                .white_space = Some(value);
        } else if node_is_equal_xsd(t, b"pattern") {
            let value = schema_restriction_var_char(t)?;
            ty_mut(ctx, cur)?
                .restrictions
                .get_or_insert_with(Box::default)
                .pattern = Some(value);
        } else if node_is_equal_xsd(t, b"enumeration") {
            let value = schema_restriction_var_char(t)?;
            let key = Key::Str(value.value.clone());
            ty_mut(ctx, cur)?
                .restrictions
                .get_or_insert_with(Box::default)
                .enumeration
                .get_or_insert_with(Table::new)
                .add(key, value);
        } else {
            break;
        }
        trav = t.next();
    }
    if !simple_type {
        trav = attribute_tail(ctx, tns, trav, cur, "restriction")?;
    }
    match trav {
        Some(t) => Err(unexpected(t, "restriction")),
        None => Ok(()),
    }
}

/// The `(group | all | choice | sequence)?` of a complex type, a
/// complex restriction or extension: the node after it.
fn particle_head(ctx: &mut Ctx, tns: Node, trav: Option<Node>, cur: TypeId) -> Res<Option<Node>> {
    let Some(t) = trav else {
        return Ok(None);
    };
    if node_is_equal_xsd(t, b"group") {
        schema_group(ctx, tns, t, Some(cur), None)?;
    } else if node_is_equal_xsd(t, b"all") {
        schema_all(ctx, tns, t, cur, None)?;
    } else if node_is_equal_xsd(t, b"choice") {
        schema_choice(ctx, tns, t, cur, None)?;
    } else if node_is_equal_xsd(t, b"sequence") {
        schema_sequence(ctx, tns, t, cur, None)?;
    } else {
        return Ok(trav);
    }
    Ok(t.next())
}

/// `schema_restriction_complexContent`.
fn schema_restriction_complex_content(
    ctx: &mut Ctx,
    tns: Node,
    rest: Node,
    cur: TypeId,
) -> Res<()> {
    let Some(base) = attribute(rest, b"base") else {
        return Err(schema_error("restriction has no 'base' attribute"));
    };
    base_encoder(ctx, rest, base, cur)?;
    let trav = particle_head(ctx, tns, after_annotation(rest), cur)?;
    match attribute_tail(ctx, tns, trav, cur, "restriction")? {
        Some(t) => Err(unexpected(t, "restriction")),
        None => Ok(()),
    }
}

/// The `fixed` attribute of a facet: `true` or `1`.
fn facet_fixed(val: Node) -> bool {
    attribute(val, b"fixed").is_some_and(|f| {
        let c = attr_content(f);
        c == b"true" || c == b"1"
    })
}

/// `schema_restriction_var_int`.
fn schema_restriction_var_int(val: Node) -> Res<RestrictionInt> {
    let fixed = facet_fixed(val);
    let Some(value) = attribute(val, b"value") else {
        return Err(schema_error("missing restriction value"));
    };
    Ok(RestrictionInt {
        value: atoi(&attr_content(value)),
        fixed,
    })
}

/// `schema_restriction_var_char`.
fn schema_restriction_var_char(val: Node) -> Res<RestrictionChar> {
    let fixed = facet_fixed(val);
    let Some(value) = attribute(val, b"value") else {
        return Err(schema_error("missing restriction value"));
    };
    Ok(RestrictionChar {
        value: attr_content(value),
        fixed,
    })
}

/// `schema_extension_simpleContent`.
fn schema_extension_simple_content(ctx: &mut Ctx, tns: Node, ext: Node, cur: TypeId) -> Res<()> {
    let Some(base) = attribute(ext, b"base") else {
        return Err(schema_error("extension has no 'base' attribute"));
    };
    base_encoder(ctx, ext, base, cur)?;
    match attribute_tail(ctx, tns, after_annotation(ext), cur, "extension")? {
        Some(t) => Err(unexpected(t, "extension")),
        None => Ok(()),
    }
}

/// `schema_extension_complexContent`.
fn schema_extension_complex_content(ctx: &mut Ctx, tns: Node, ext: Node, cur: TypeId) -> Res<()> {
    let Some(base) = attribute(ext, b"base") else {
        return Err(schema_error("extension has no 'base' attribute"));
    };
    base_encoder(ctx, ext, base, cur)?;
    let trav = particle_head(ctx, tns, after_annotation(ext), cur)?;
    match attribute_tail(ctx, tns, trav, cur, "extension")? {
        Some(t) => Err(unexpected(t, "extension")),
        None => Ok(()),
    }
}

/// `schema_min_max`: `minOccurs` and `maxOccurs`, each 1 where absent,
/// `unbounded` -1.
pub(crate) fn schema_min_max(node: Node, model: &mut ContentModel) {
    model.min_occurs = attribute(node, b"minOccurs").map_or(1, |a| atoi(&attr_content(a)));
    model.max_occurs = attribute(node, b"maxOccurs").map_or(1, |a| {
        let c = attr_content(a);
        if c == b"unbounded" { -1 } else { atoi(&c) }
    });
}

/// A model of `kind` over particles, its occurrences not yet read.
fn particles(kind: ContentKind) -> ContentModel {
    ContentModel {
        kind,
        min_occurs: 0,
        max_occurs: 0,
        u: Content::Particles(Vec::new()),
    }
}

/// `zend_hash_next_index_insert_ptr(model->u.content, new_model)`.
fn push_particle(model: &mut ContentModel, new_model: ContentModel) {
    if let Content::Particles(list) = &mut model.u {
        list.push(new_model);
    }
}

/// A model read into: the type's own where no outer one was given,
/// else the next particle of the outer one.
fn attach(
    ctx: &mut Ctx,
    cur: TypeId,
    model: Option<&mut ContentModel>,
    new_model: ContentModel,
) -> Res<()> {
    match model {
        None => ty_mut(ctx, cur)?.model = Some(Box::new(new_model)),
        Some(outer) => push_particle(outer, new_model),
    }
    Ok(())
}

/// `schema_all`.
fn schema_all(
    ctx: &mut Ctx,
    tns: Node,
    all: Node,
    cur: TypeId,
    model: Option<&mut ContentModel>,
) -> Res<()> {
    let mut new_model = particles(ContentKind::All);
    schema_min_max(all, &mut new_model);
    let mut trav = after_annotation(all);
    while let Some(t) = trav {
        if node_is_equal_xsd(t, b"element") {
            schema_element(ctx, tns, t, Some(cur), Some(&mut new_model))?;
        } else {
            return Err(unexpected(t, "all"));
        }
        trav = t.next();
    }
    attach(ctx, cur, model, new_model)
}

/// `schema_group`: a named group (a type of `sdl->groups` where it is
/// global) or a reference to one, which pass two resolves.
fn schema_group(
    ctx: &mut Ctx,
    tns: Node,
    group: Node,
    cur_type: Option<TypeId>,
    model: Option<&mut ContentModel>,
) -> Res<()> {
    let ns = attribute(group, b"targetNamespace").unwrap_or(tns);
    let mut name = attribute(group, b"name");
    let mut ref_ = None;
    if name.is_none() {
        ref_ = attribute(group, b"ref");
        name = ref_;
    }
    let Some(name) = name else {
        return Err(schema_error("group has no 'name' nor 'ref' attributes"));
    };
    let (mut new_model, key) = if let Some(r) = ref_ {
        let value = attr_content(r);
        let (type_, prefix) = parse_namespace(&value);
        let mut key = match group.search_ns(prefix) {
            Some(n) => href(n),
            None => attr_content(attribute(group, b"targetNamespace").unwrap_or(tns)),
        };
        key.push(b':');
        key.extend_from_slice(type_);
        let m = ContentModel {
            kind: ContentKind::GroupRef,
            min_occurs: 0,
            max_occurs: 0,
            u: Content::GroupRef(key.clone()),
        };
        (m, key)
    } else {
        (
            particles(ContentKind::Sequence),
            join(&attr_content(ns), &attr_content(name)),
        )
    };
    let cur = if let Some(c) = cur_type {
        c
    } else {
        let id = ctx.sdl.new_type(Type::default());
        let groups = ctx.sdl.groups.get_or_insert_with(Table::new);
        if !groups.add(Key::Str(key.clone()), id) {
            return Err(schema_error(&format!(
                "group '{}' already defined",
                lossy(&key)
            )));
        }
        id
    };
    schema_min_max(group, &mut new_model);

    let trav = after_annotation(group);
    if let Some(t) = trav {
        let kind = if node_is_equal_xsd(t, b"choice") {
            ContentKind::Choice
        } else if node_is_equal_xsd(t, b"sequence") {
            ContentKind::Sequence
        } else if node_is_equal_xsd(t, b"all") {
            ContentKind::All
        } else {
            return Err(unexpected(t, "group"));
        };
        if ref_.is_some() {
            return Err(schema_error(
                "group has both 'ref' attribute and subcontent",
            ));
        }
        new_model.kind = kind;
        match kind {
            ContentKind::Choice => schema_choice(ctx, tns, t, cur, Some(&mut new_model))?,
            ContentKind::Sequence => schema_sequence(ctx, tns, t, cur, Some(&mut new_model))?,
            _ => schema_all(ctx, tns, t, cur, Some(&mut new_model))?,
        }
        if let Some(n) = t.next() {
            return Err(unexpected(n, "group"));
        }
    }
    attach(ctx, cur, model, new_model)
}

/// The particles of a `<choice>` or a `<sequence>`.
fn nested_particles(
    ctx: &mut Ctx,
    tns: Node,
    node: Node,
    cur: TypeId,
    new_model: &mut ContentModel,
    within: &str,
) -> Res<()> {
    let mut trav = after_annotation(node);
    while let Some(t) = trav {
        if node_is_equal_xsd(t, b"element") {
            schema_element(ctx, tns, t, Some(cur), Some(new_model))?;
        } else if node_is_equal_xsd(t, b"group") {
            schema_group(ctx, tns, t, Some(cur), Some(new_model))?;
        } else if node_is_equal_xsd(t, b"choice") {
            schema_choice(ctx, tns, t, cur, Some(new_model))?;
        } else if node_is_equal_xsd(t, b"sequence") {
            schema_sequence(ctx, tns, t, cur, Some(new_model))?;
        } else if node_is_equal_xsd(t, b"any") {
            schema_any(t, Some(new_model));
        } else {
            return Err(unexpected(t, within));
        }
        trav = t.next();
    }
    Ok(())
}

/// `schema_choice`.
fn schema_choice(
    ctx: &mut Ctx,
    tns: Node,
    choice: Node,
    cur: TypeId,
    model: Option<&mut ContentModel>,
) -> Res<()> {
    let mut new_model = particles(ContentKind::Choice);
    schema_min_max(choice, &mut new_model);
    nested_particles(ctx, tns, choice, cur, &mut new_model, "choice")?;
    attach(ctx, cur, model, new_model)
}

/// `schema_sequence`.
fn schema_sequence(
    ctx: &mut Ctx,
    tns: Node,
    seq: Node,
    cur: TypeId,
    model: Option<&mut ContentModel>,
) -> Res<()> {
    let mut new_model = particles(ContentKind::Sequence);
    schema_min_max(seq, &mut new_model);
    nested_particles(ctx, tns, seq, cur, &mut new_model, "sequence")?;
    attach(ctx, cur, model, new_model)
}

/// `schema_any`: an `XSD_CONTENT_ANY` particle, where there is a model
/// to put it in.
fn schema_any(any: Node, model: Option<&mut ContentModel>) {
    if let Some(model) = model {
        let mut new_model = ContentModel {
            kind: ContentKind::Any,
            min_occurs: 0,
            max_occurs: 0,
            u: Content::None,
        };
        schema_min_max(any, &mut new_model);
        push_particle(model, new_model);
    }
}

/// `schema_complexContent`.
fn schema_complex_content(ctx: &mut Ctx, tns: Node, node: Node, cur: TypeId) -> Res<()> {
    let Some(t) = after_annotation(node) else {
        return Err(schema_error(
            "<restriction> or <extension> expected in complexContent",
        ));
    };
    if node_is_equal_xsd(t, b"restriction") {
        ty_mut(ctx, cur)?.kind = TypeKind::Restriction;
        schema_restriction_complex_content(ctx, tns, t, cur)?;
    } else if node_is_equal_xsd(t, b"extension") {
        ty_mut(ctx, cur)?.kind = TypeKind::Extension;
        schema_extension_complex_content(ctx, tns, t, cur)?;
    } else {
        return Err(unexpected(t, "complexContent"));
    }
    match t.next() {
        Some(n) => Err(unexpected(n, "complexContent")),
        None => Ok(()),
    }
}

/// `schema_complexType`.
fn schema_complex_type(ctx: &mut Ctx, tns: Node, comp: Node, cur_type: Option<TypeId>) -> Res<()> {
    let ns = attribute(comp, b"targetNamespace").unwrap_or(tns);
    let name = attribute(comp, b"name");
    let cur = if let Some(cur) = cur_type {
        anonymous_type(ctx, TypeKind::Complex, name, ns, cur)?
    } else if let Some(name) = name {
        let (n, nsv) = (attr_content(name), attr_content(ns));
        let ptr = push_type(
            &mut ctx.sdl,
            Type {
                kind: TypeKind::Complex,
                name: Some(n.clone()),
                namens: Some(nsv.clone()),
                ..Type::default()
            },
        );
        create_encoder(&mut ctx.sdl, Some(ptr), &nsv, &n);
        ptr
    } else {
        return Err(schema_error("complexType has no 'name' attribute"));
    };

    let mut trav = after_annotation(comp);
    if let Some(t) = trav {
        if node_is_equal_xsd(t, b"simpleContent") {
            schema_simple_content(ctx, tns, t, cur)?;
            trav = t.next();
        } else if node_is_equal_xsd(t, b"complexContent") {
            schema_complex_content(ctx, tns, t, cur)?;
            trav = t.next();
        } else {
            let next = particle_head(ctx, tns, trav, cur)?;
            trav = attribute_tail(ctx, tns, next, cur, "complexType")?;
        }
    }
    match trav {
        Some(t) => Err(unexpected(t, "complexType")),
        None => Ok(()),
    }
}

/// The `form` attribute, and where it is absent or neither value, the
/// enclosing schema's `elementFormDefault` or `attributeFormDefault`.
fn read_form(node: Node, default_attr: &[u8]) -> Form {
    let form = attribute(node, b"form").map_or(Form::Default, |a| {
        let c = attr_content(a);
        if c == b"qualified" {
            Form::Qualified
        } else if c == b"unqualified" {
            Form::Unqualified
        } else {
            Form::Default
        }
    });
    if form != Form::Default {
        return form;
    }
    let mut parent = node.parent();
    while let Some(p) = parent {
        if node_is_equal_xsd(p, b"schema") {
            return match attribute(p, default_attr) {
                Some(d) if attr_content(d) == b"qualified" => Form::Qualified,
                _ => Form::Unqualified,
            };
        }
        parent = p.parent();
    }
    Form::Unqualified
}

/// `schema_element`.
#[allow(
    clippy::too_many_lines,
    reason = "php's function, one Rust function per C function"
)]
fn schema_element(
    ctx: &mut Ctx,
    tns: Node,
    element: Node,
    cur_type: Option<TypeId>,
    model: Option<&mut ContentModel>,
) -> Res<()> {
    let ns = attribute(element, b"targetNamespace").unwrap_or(tns);
    let mut name = attribute(element, b"name");
    let mut ref_ = None;
    if name.is_none() {
        ref_ = attribute(element, b"ref");
        name = ref_;
    }
    let Some(name) = name else {
        return Err(schema_error("element has no 'name' nor 'ref' attributes"));
    };
    let mut new_type = Type::default();
    if let Some(r) = ref_ {
        let value = attr_content(r);
        let (type_, prefix) = parse_namespace(&value);
        let mut cat = match element.search_ns(prefix) {
            Some(n) => {
                let h = href(n);
                new_type.namens = Some(h.clone());
                h
            }
            None => attr_content(attribute(element, b"targetNamespace").unwrap_or(tns)),
        };
        cat.push(b':');
        cat.extend_from_slice(type_);
        new_type.name = Some(type_.to_vec());
        new_type.ref_ = Some(cat);
    } else {
        new_type.name = Some(attr_content(name));
        new_type.namens = Some(attr_content(ns));
    }
    new_type.nillable = false;
    let key = match cur_type {
        None => join(
            new_type.namens.as_deref().unwrap_or_default(),
            new_type.name.as_deref().unwrap_or_default(),
        ),
        Some(_) => new_type.name.clone().unwrap_or_default(),
    };
    let id = ctx.sdl.new_type(new_type);
    match cur_type {
        None => {
            let elements = ctx.sdl.elements.get_or_insert_with(Table::new);
            if !elements.add(Key::Str(key.clone()), id) {
                return Err(schema_error(&format!(
                    "element '{}' already defined",
                    lossy(&key)
                )));
            }
        }
        Some(c) => {
            let elements = ty_mut(ctx, c)?.elements.get_or_insert_with(Table::new);
            if !elements.add(Key::Str(key), id) {
                elements.push(id);
            }
        }
    }
    if let Some(model) = model {
        let mut new_model = ContentModel {
            kind: ContentKind::Element,
            min_occurs: 0,
            max_occurs: 0,
            u: Content::Element(id),
        };
        schema_min_max(element, &mut new_model);
        push_particle(model, new_model);
    }
    let cur = id;

    let nillable = match attribute(element, b"nillable") {
        Some(a) => {
            if ref_.is_some() {
                return Err(schema_error(
                    "element has both 'ref' and 'nillable' attributes",
                ));
            }
            let c = attr_content(a);
            c.eq_ignore_ascii_case(b"true") || c.eq_ignore_ascii_case(b"1")
        }
        None => false,
    };
    ty_mut(ctx, cur)?.nillable = nillable;
    if let Some(a) = attribute(element, b"fixed") {
        if ref_.is_some() {
            return Err(schema_error(
                "element has both 'ref' and 'fixed' attributes",
            ));
        }
        ty_mut(ctx, cur)?.fixed = Some(attr_content(a));
    }
    if let Some(a) = attribute(element, b"default") {
        if ref_.is_some() {
            return Err(schema_error(
                "element has both 'default' and 'fixed' attributes",
            ));
        }
        ty_mut(ctx, cur)?.def = Some(attr_content(a));
    }
    ty_mut(ctx, cur)?.form = read_form(element, b"elementFormDefault");

    let type_attr = attribute(element, b"type");
    if let Some(t) = type_attr {
        if ref_.is_some() {
            return Err(schema_error("element has both 'ref' and 'type' attributes"));
        }
        let value = attr_content(t);
        let (cptype, prefix) = parse_namespace(&value);
        if let Some(nsptr) = element.search_ns(prefix) {
            let enc = get_create_encoder(&mut ctx.sdl, Some(cur), &href(nsptr), cptype);
            ty_mut(ctx, cur)?.encode = Some(enc);
        }
    }

    let mut trav = after_annotation(element);
    if let Some(t) = trav {
        let simple = node_is_equal_xsd(t, b"simpleType");
        if simple || node_is_equal_xsd(t, b"complexType") {
            if ref_.is_some() {
                return Err(schema_error("element has both 'ref' attribute and subtype"));
            }
            if type_attr.is_some() {
                return Err(schema_error(
                    "element has both 'type' attribute and subtype",
                ));
            }
            if simple {
                schema_simple_type(ctx, tns, t, Some(cur))?;
            } else {
                schema_complex_type(ctx, tns, t, Some(cur))?;
            }
            trav = t.next();
        }
    }
    while let Some(t) = trav {
        if !node_is_equal_xsd(t, b"unique")
            && !node_is_equal_xsd(t, b"key")
            && !node_is_equal_xsd(t, b"keyref")
        {
            return Err(unexpected(t, "element"));
        }
        trav = t.next();
    }
    Ok(())
}

/// The `use` attribute's value.
fn read_use(c: &[u8]) -> AttrUse {
    match c {
        b"prohibited" => AttrUse::Prohibited,
        b"required" => AttrUse::Required,
        b"optional" => AttrUse::Optional,
        _ => AttrUse::Default,
    }
}

/// `schema_attribute`: an attribute of `cur_type`, or a global one where
/// none is given.
#[allow(
    clippy::too_many_lines,
    reason = "php's function, one Rust function per C function"
)]
fn schema_attribute(
    ctx: &mut Ctx,
    tns: Node,
    attr_type: Node,
    cur_type: Option<TypeId>,
) -> Res<()> {
    let mut name = attribute(attr_type, b"name");
    let mut ref_ = None;
    if name.is_none() {
        ref_ = attribute(attr_type, b"ref");
        name = ref_;
    }
    if name.is_none() {
        return Err(schema_error("attribute has no 'name' nor 'ref' attributes"));
    }
    let mut new_attr = Attribute::default();
    let mut key;
    if let Some(r) = ref_ {
        let value = attr_content(r);
        let (attr_name, prefix) = parse_namespace(&value);
        key = match attr_type.search_ns(prefix) {
            Some(n) => {
                let h = href(n);
                new_attr.namens = Some(h.clone());
                h
            }
            None => attr_content(attribute(attr_type, b"targetNamespace").unwrap_or(tns)),
        };
        key.push(b':');
        key.extend_from_slice(attr_name);
        new_attr.ref_ = Some(key.clone());
    } else {
        let ns = attribute(attr_type, b"targetNamespace").unwrap_or(tns);
        let nsv = attr_content(ns);
        key = nsv.clone();
        key.push(b':');
        new_attr.namens = Some(nsv);
        key.extend_from_slice(&name.map(attr_content).unwrap_or_default());
    }
    let taken = match cur_type {
        None => ctx
            .attributes
            .as_ref()
            .is_some_and(|t| t.contains(&Key::Str(key.clone()))),
        Some(c) => ty_mut(ctx, c)?
            .attributes
            .as_ref()
            .is_some_and(|t| t.contains(&Key::Str(key.clone()))),
    };
    if taken {
        return Err(schema_error(&format!(
            "attribute '{}' already defined",
            lossy(&key)
        )));
    }

    let type_attr = attribute(attr_type, b"type");
    if let Some(t) = type_attr {
        if ref_.is_some() {
            return Err(schema_error(
                "attribute has both 'ref' and 'type' attributes",
            ));
        }
        let value = attr_content(t);
        let (cptype, prefix) = parse_namespace(&value);
        if let Some(nsptr) = attr_type.search_ns(prefix) {
            new_attr.encode = Some(get_create_encoder(
                &mut ctx.sdl,
                cur_type,
                &href(nsptr),
                cptype,
            ));
        }
    }

    let mut attr = attr_type.properties();
    while let Some(a) = attr {
        if attr_is_equal(a, b"default", None) {
            new_attr.def = Some(attr_content(a));
        } else if attr_is_equal(a, b"fixed", None) {
            new_attr.fixed = Some(attr_content(a));
        } else if attr_is_equal(a, b"form", None) {
            let c = attr_content(a);
            new_attr.form = if c == b"qualified" {
                Form::Qualified
            } else if c == b"unqualified" {
                Form::Unqualified
            } else {
                Form::Default
            };
        } else if attr_is_equal(a, b"name", None) {
            new_attr.name = Some(attr_content(a));
        } else if attr_is_equal(a, b"use", None) {
            new_attr.use_ = read_use(&attr_content(a));
        } else if !attr_is_equal(a, b"id", None)
            && !attr_is_equal(a, b"ref", None)
            && !attr_is_equal(a, b"type", None)
            && let Some(ns_ptr) = a.ns()
            && href(ns_ptr) != XSD_NAMESPACE
        {
            let value = attr_content(a);
            let (local, prefix) = parse_namespace(&value);
            let ext = match a.parent().and_then(|p| p.search_ns(prefix)) {
                Some(n) => ExtraAttribute {
                    ns: Some(href(n)),
                    val: Some(local.to_vec()),
                },
                None => ExtraAttribute {
                    ns: None,
                    val: Some(value.clone()),
                },
            };
            let key2 = join(&href(ns_ptr), &a.name());
            new_attr
                .extra_attributes
                .get_or_insert_with(Table::new)
                .add(Key::Str(key2), ext);
        }
        attr = a.next();
    }
    if new_attr.form == Form::Default {
        new_attr.form = read_form_default(attr_type);
    }

    let mut trav = after_annotation(attr_type);
    if let Some(t) = trav
        && node_is_equal_xsd(t, b"simpleType")
    {
        if ref_.is_some() {
            return Err(schema_error(
                "attribute has both 'ref' attribute and subtype",
            ));
        }
        if type_attr.is_some() {
            return Err(schema_error(
                "attribute has both 'type' attribute and subtype",
            ));
        }
        let dummy = ctx.sdl.new_type(Type {
            name: Some(anonymous_name(&ctx.sdl)),
            namens: Some(attr_content(tns)),
            ..Type::default()
        });
        schema_simple_type(ctx, tns, t, Some(dummy))?;
        new_attr.encode = ty_mut(ctx, dummy)?.encode;
        trav = t.next();
    }
    if let Some(t) = trav {
        return Err(unexpected(t, "attribute"));
    }
    let table = match cur_type {
        None => ctx.attributes.get_or_insert_with(Table::new),
        Some(c) => ty_mut(ctx, c)?.attributes.get_or_insert_with(Table::new),
    };
    table.add(Key::Str(key), new_attr);
    Ok(())
}

/// An attribute's form where it named none: the enclosing schema's
/// `attributeFormDefault`.
fn read_form_default(node: Node) -> Form {
    let mut parent = node.parent();
    while let Some(p) = parent {
        if node_is_equal_xsd(p, b"schema") {
            return match attribute(p, b"attributeFormDefault") {
                Some(d) if attr_content(d) == b"qualified" => Form::Qualified,
                _ => Form::Unqualified,
            };
        }
        parent = p.parent();
    }
    Form::Unqualified
}

/// `schema_attributeGroup`: a global group (in `ctx->attributeGroups`), or a reference to one inside a type, which pass
/// two expands.
fn schema_attribute_group(
    ctx: &mut Ctx,
    tns: Node,
    group: Node,
    cur_type: Option<TypeId>,
) -> Res<()> {
    let mut name = attribute(group, b"name");
    let mut ref_ = None;
    if name.is_none() {
        ref_ = attribute(group, b"ref");
        name = ref_;
    }
    let Some(name) = name else {
        return Err(schema_error(
            "attributeGroup has no 'name' nor 'ref' attributes",
        ));
    };
    let mut cur = cur_type;
    match cur_type {
        None => {
            let ns = attribute(group, b"targetNamespace").unwrap_or(tns);
            let (n, nsv) = (attr_content(name), attr_content(ns));
            let key = join(&nsv, &n);
            let id = ctx.sdl.new_type(Type {
                name: Some(n),
                namens: Some(nsv),
                ..Type::default()
            });
            let groups = ctx.attribute_groups.get_or_insert_with(Table::new);
            if !groups.add(Key::Str(key.clone()), id) {
                return Err(schema_error(&format!(
                    "attributeGroup '{}' already defined",
                    lossy(&key)
                )));
            }
            cur = Some(id);
        }
        Some(c) => {
            if let Some(r) = ref_ {
                let value = attr_content(r);
                let (group_name, prefix) = parse_namespace(&value);
                let mut key = group.search_ns(prefix).map(href).unwrap_or_default();
                key.push(b':');
                key.extend_from_slice(group_name);
                let new_attr = Attribute {
                    ref_: Some(key),
                    ..Attribute::default()
                };
                ty_mut(ctx, c)?
                    .attributes
                    .get_or_insert_with(Table::new)
                    .push(new_attr);
                cur = None;
            }
        }
    }

    let mut trav = after_annotation(group);
    while let Some(t) = trav {
        let is_attr = node_is_equal_xsd(t, b"attribute");
        let is_group = node_is_equal_xsd(t, b"attributeGroup");
        let is_any = node_is_equal_xsd(t, b"anyAttribute");
        if !is_attr && !is_group && !is_any {
            return Err(unexpected(t, "attributeGroup"));
        }
        if ref_.is_some() {
            return Err(schema_error(
                "attributeGroup has both 'ref' attribute and subattribute",
            ));
        }
        if is_any {
            trav = t.next();
            break;
        }
        if is_attr {
            schema_attribute(ctx, tns, t, cur)?;
        } else {
            schema_attribute_group(ctx, tns, t, cur)?;
        }
        trav = t.next();
    }
    match trav {
        Some(t) => Err(unexpected(t, "attributeGroup")),
        None => Ok(()),
    }
}

// ---- pass two

/// `schema_find_by_ref`: the key of the row named `ref`, else of the one
/// named by its tail from the last colon on, the colon kept.
fn schema_find_by_ref<V>(ht: &Table<V>, r: &[u8]) -> Option<Key> {
    let key = Key::Str(r.to_vec());
    if ht.contains(&key) {
        return Some(key);
    }
    let at = r.iter().rposition(|c| *c == b':')?;
    let tail = Key::Str(r[at..].to_vec());
    ht.contains(&tail).then_some(tail)
}

/// `schema_attribute_fixup`: a `ref` resolved against the global
/// attributes, what the attribute left unsaid taken from the one it
/// names, and its name from the reference where it has none.
fn schema_attribute_fixup(ctx: &mut Ctx, attr: &mut Attribute) {
    let Some(r) = attr.ref_.clone() else {
        return;
    };
    let found = ctx.attributes.as_ref().and_then(|t| {
        schema_find_by_ref(t, &r).and_then(|k| Some((k.clone(), t.get(&k)?.clone())))
    });
    if let Some((key, mut tmp)) = found {
        schema_attribute_fixup(ctx, &mut tmp);
        if let Some(t) = ctx.attributes.as_mut() {
            t.update(key, tmp.clone());
        }
        if tmp.name.is_some() && attr.name.is_none() {
            attr.name.clone_from(&tmp.name);
        }
        if tmp.namens.is_some() && attr.namens.is_none() {
            attr.namens.clone_from(&tmp.namens);
        }
        if tmp.def.is_some() && attr.def.is_none() {
            attr.def.clone_from(&tmp.def);
        }
        if tmp.fixed.is_some() && attr.fixed.is_none() {
            attr.fixed.clone_from(&tmp.fixed);
        }
        if attr.form == Form::Default {
            attr.form = tmp.form;
        }
        if attr.use_ == AttrUse::Default {
            attr.use_ = tmp.use_;
        }
        if tmp.extra_attributes.is_some() {
            attr.extra_attributes.clone_from(&tmp.extra_attributes);
        }
        attr.encode = tmp.encode;
    }
    if attr.name.is_none() {
        attr.name = Some(match r.iter().rposition(|c| *c == b':') {
            Some(at) => r[at + 1..].to_vec(),
            None => r.clone(),
        });
    }
    attr.ref_ = None;
}

/// The key of the row `n` rows from the start, in order.
fn nth_key<V>(t: &Table<V>, n: usize) -> Option<Key> {
    t.iter().nth(n).map(|(k, _)| k.clone())
}

/// `schema_attributegroup_fixup`: a reference to an attribute group
/// expanded into `ht`, each attribute of the group fixed up and copied
/// under its key, each reference in the group expanded in turn and
/// dropped from the group.
fn schema_attributegroup_fixup(
    ctx: &mut Ctx,
    attr: &Attribute,
    ht: &mut Table<Attribute>,
) -> Res<()> {
    let Some(r) = attr.ref_.as_deref() else {
        return Ok(());
    };
    let Some(id) = ctx
        .attribute_groups
        .as_ref()
        .and_then(|g| schema_find_by_ref(g, r).and_then(|k| g.get(&k).copied()))
    else {
        return Ok(());
    };
    if !ctx.fixup_in_progress.insert(id) {
        return Err(schema_error(&format!(
            "recursive attributeGroup 'ref' attribute '{}'",
            lossy(r)
        )));
    }
    let expanded = expand_attribute_group(ctx, id, ht);
    ctx.fixup_in_progress.remove(&id);
    expanded
}

/// The body of [`schema_attributegroup_fixup`] over the group `id`, while
/// it is marked in progress.
fn expand_attribute_group(ctx: &mut Ctx, id: TypeId, ht: &mut Table<Attribute>) -> Res<()> {
    let Some(mut attrs) = ctx.sdl.ty_mut(id).and_then(|t| t.attributes.take()) else {
        return Ok(());
    };
    let mut n = 0;
    while let Some(key) = nth_key(&attrs, n) {
        let Some(mut tmp_attr) = attrs.get(&key).cloned() else {
            break;
        };
        if let Key::Str(_) = key {
            schema_attribute_fixup(ctx, &mut tmp_attr);
            attrs.update(key.clone(), tmp_attr.clone());
            ht.add(key, tmp_attr);
            n += 1;
        } else {
            let expanded = schema_attributegroup_fixup(ctx, &tmp_attr, ht);
            attrs.remove(&key);
            if expanded.is_err() {
                if let Some(t) = ctx.sdl.ty_mut(id) {
                    t.attributes = Some(attrs);
                }
                return expanded;
            }
        }
    }
    if let Some(t) = ctx.sdl.ty_mut(id) {
        t.attributes = Some(attrs);
    }
    Ok(())
}

/// `schema_content_model_fixup`: a group reference resolved, and a
/// `<choice>` that repeats made an `<all>` of optional repeating
/// particles.
fn schema_content_model_fixup(ctx: &mut Ctx, model: &mut ContentModel) -> Res<()> {
    match model.kind {
        ContentKind::GroupRef => {
            let Content::GroupRef(r) = &model.u else {
                return Ok(());
            };
            let found = ctx.sdl.groups.as_ref().and_then(|g| g.get_str(r).copied());
            match found {
                Some(id) => {
                    if ctx.fixup_in_progress.contains(&id) {
                        return Err(schema_error(&format!(
                            "recursive group 'ref' attribute '{}'",
                            lossy(r)
                        )));
                    }
                    schema_type_fixup(ctx, id)?;
                    model.kind = ContentKind::Group;
                    model.u = Content::Group(id);
                }
                None => {
                    return Err(schema_error(&format!(
                        "unresolved group 'ref' attribute '{}'",
                        lossy(r)
                    )));
                }
            }
        }
        ContentKind::Choice | ContentKind::Sequence | ContentKind::All => {
            if model.kind == ContentKind::Choice && model.max_occurs != 1 {
                if let Content::Particles(list) = &mut model.u {
                    for tmp in list {
                        tmp.min_occurs = 0;
                        tmp.max_occurs = model.max_occurs;
                    }
                }
                model.kind = ContentKind::All;
                model.min_occurs = 1;
                model.max_occurs = 1;
            }
            if let Content::Particles(list) = &mut model.u {
                for tmp in list {
                    schema_content_model_fixup(ctx, tmp)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// `schema_type_fixup`: an element `ref` resolved, then the type's
/// elements, its model and its attributes, an attribute group reference
/// expanded in place of itself; the type marked in progress meanwhile,
/// so a group or an attribute group that names itself is refused rather
/// than expanded into itself.
///
/// ```php
/// foreach (['<group name="A"><sequence><element name="a" type="string"/><group ref="tns:A"/></sequence></group><element name="root"><complexType><group ref="tns:A"/></complexType></element>',
///           '<attributeGroup name="A"><attribute name="a" type="string"/><attributeGroup ref="tns:A"/></attributeGroup><element name="root"><complexType><attributeGroup ref="tns:A"/></complexType></element>'] as $schema) {
///     $wsdl = '<definitions xmlns="http://schemas.xmlsoap.org/wsdl/" xmlns:tns="urn:test"><types><xsd:schema xmlns:xsd="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:test" xmlns="http://www.w3.org/2001/XMLSchema">' . $schema . '</xsd:schema></types></definitions>';
///     try { new SoapClient('data://text/plain,' . urlencode($wsdl), ['cache_wsdl' => WSDL_CACHE_NONE]); echo "parsed\n"; }
///     catch (SoapFault $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// SOAP-ERROR: Parsing Schema: recursive group 'ref' attribute 'urn:test:A'
/// SOAP-ERROR: Parsing Schema: recursive attributeGroup 'ref' attribute 'urn:test:A'
/// ```
fn schema_type_fixup(ctx: &mut Ctx, id: TypeId) -> Res<()> {
    ctx.fixup_in_progress.insert(id);
    let fixed = type_fixup(ctx, id);
    ctx.fixup_in_progress.remove(&id);
    fixed
}

/// The body of [`schema_type_fixup`].
fn type_fixup(ctx: &mut Ctx, id: TypeId) -> Res<()> {
    let Some(r) = ctx.sdl.ty(id).and_then(|t| t.ref_.clone()) else {
        return fixup_parts(ctx, id);
    };
    if let Some(elements) = ctx.sdl.elements.as_ref() {
        let found = schema_find_by_ref(elements, &r).and_then(|k| elements.get(&k).copied());
        if let Some(tmp) = found {
            let Some((kind, encode, nillable, fixed, def, form)) = ctx.sdl.ty(tmp).map(|s| {
                (
                    s.kind,
                    s.encode,
                    s.nillable,
                    s.fixed.clone(),
                    s.def.clone(),
                    s.form,
                )
            }) else {
                return Err(schema_error("internal error: a type out of range"));
            };
            let t = ty_mut(ctx, id)?;
            t.kind = kind;
            t.encode = encode;
            if nillable {
                t.nillable = true;
            }
            if fixed.is_some() {
                t.fixed = fixed;
            }
            if def.is_some() {
                t.def = def;
            }
            t.form = form;
        } else if r == [XSD_NAMESPACE, b":schema"].concat() {
            ty_mut(ctx, id)?.encode = get_conversion(ty::XSD_ANYXML);
        } else {
            return Err(schema_error(&format!(
                "unresolved element 'ref' attribute '{}'",
                lossy(&r)
            )));
        }
    }
    ty_mut(ctx, id)?.ref_ = None;
    fixup_parts(ctx, id)
}

/// The elements, the model and the attributes of [`schema_type_fixup`].
fn fixup_parts(ctx: &mut Ctx, id: TypeId) -> Res<()> {
    let elements: Vec<TypeId> = ctx
        .sdl
        .ty(id)
        .and_then(|t| t.elements.as_ref())
        .map(|e| e.values().copied().collect())
        .unwrap_or_default();
    for e in elements {
        schema_type_fixup(ctx, e)?;
    }
    if let Some(mut model) = ctx.sdl.ty_mut(id).and_then(|t| t.model.take()) {
        let fixed = schema_content_model_fixup(ctx, &mut model);
        if let Some(t) = ctx.sdl.ty_mut(id) {
            t.model = Some(model);
        }
        fixed?;
    }
    let Some(mut attrs) = ctx.sdl.ty_mut(id).and_then(|t| t.attributes.take()) else {
        return Ok(());
    };
    let mut n = 0;
    while let Some(key) = nth_key(&attrs, n) {
        let Some(mut attr) = attrs.get(&key).cloned() else {
            break;
        };
        if let Key::Str(_) = key {
            schema_attribute_fixup(ctx, &mut attr);
            attrs.update(key, attr);
            n += 1;
        } else {
            let expanded = schema_attributegroup_fixup(ctx, &attr, &mut attrs);
            attrs.remove(&key);
            if expanded.is_err() {
                if let Some(t) = ctx.sdl.ty_mut(id) {
                    t.attributes = Some(attrs);
                }
                return expanded;
            }
        }
    }
    if let Some(t) = ctx.sdl.ty_mut(id) {
        t.attributes = Some(attrs);
    }
    Ok(())
}

/// `schema_pass2`: the global attributes, the attribute groups, the
/// elements, the groups and the types fixed up in that order, then the
/// `ctx->attributes` and `ctx->attributeGroups` dropped.
pub(crate) fn schema_pass2(ctx: &mut Ctx) -> Res<()> {
    if let Some(keys) = ctx
        .attributes
        .as_ref()
        .map(|t| t.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>())
    {
        for key in keys {
            let Some(mut attr) = ctx.attributes.as_ref().and_then(|t| t.get(&key).cloned()) else {
                continue;
            };
            schema_attribute_fixup(ctx, &mut attr);
            if let Some(t) = ctx.attributes.as_mut() {
                t.update(key, attr);
            }
        }
    }
    let ids = |t: Option<&Table<TypeId>>| -> Vec<TypeId> {
        t.map(|t| t.values().copied().collect()).unwrap_or_default()
    };
    for id in ids(ctx.attribute_groups.as_ref()) {
        schema_type_fixup(ctx, id)?;
    }
    for id in ids(ctx.sdl.elements.as_ref()) {
        schema_type_fixup(ctx, id)?;
    }
    for id in ids(ctx.sdl.groups.as_ref()) {
        schema_type_fixup(ctx, id)?;
    }
    for id in ids(ctx.sdl.types.as_ref()) {
        schema_type_fixup(ctx, id)?;
    }
    ctx.attributes = None;
    ctx.attribute_groups = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::white_space_collapse;

    #[test]
    fn collapse_trims_and_joins() {
        assert_eq!(white_space_collapse(b"  a\t\tb \n c  "), b"a b c");
        assert_eq!(white_space_collapse(b""), b"");
    }
}
