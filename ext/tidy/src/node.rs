//! `final class tidyNode`: one node of a document's tree, its nine
//! read-only properties written once as `tidy_add_node_default_properties`
//! writes them, its children made with it, and the walks php's methods
//! make from it.

use php_ext::abi::{Instance, This};
use php_ext::attr::php_class;
use php_ext::call::{lookup_class, new_object, write_prop};
use php_ext::raising::{Thrown, fatal, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value};

use crate::doc::{NODE, NodeAt, Obj, fixed};
use crate::ffi::{
    self, NODE_ASP, NODE_COMMENT, NODE_DOCTYPE, NODE_JSTE, NODE_PHP, NODE_ROOT, NODE_TEXT, Step,
};
use crate::{DocId, generation, retain, with_doc};

/// `tidy_node_validate`: the node an object names and its document, or
/// the `Error` for an object no document made (an unserialized one) and
/// for one read from a tree its document has since reparsed.
///
/// ```php
/// $n = unserialize('O:8:"tidyNode":0:{}');
/// try { $n->hasChildren(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $t = tidy_parse_string('<p>one</p><p>two</p>');
/// $n = $t->body()->child[0];
/// var_dump($n->isHtml(), $n->hasSiblings());
/// $t->parseString('<p>three</p>');
/// foreach ([fn() => (string) $n, fn() => $n->isText(), fn() => $n->getParent()] as $f) {
///     try { $f(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// var_dump($n->name, $t->body()->child[0]->isHtml());
/// ```
/// ```expect
/// tidyNode object is not initialized
/// bool(true)
/// bool(true)
/// tidyNode object is no longer valid after its document was reparsed
/// tidyNode object is no longer valid after its document was reparsed
/// tidyNode object is no longer valid after its document was reparsed
/// string(1) "p"
/// bool(true)
/// ```
fn of(req: &mut Request, object: PhpObject) -> Result<(DocId, ffi::Node), Thrown> {
    let named = object
        .native_mut::<Obj>(req)
        .and_then(|obj| Some((obj.doc?, obj.node?)));
    let Some((id, at)) = named else {
        return Err(throw_error(
            req,
            "Error",
            "tidyNode object is not initialized",
        ));
    };
    if generation(req, id) != Some(at.generation) {
        return Err(throw_error(
            req,
            "Error",
            "tidyNode object is no longer valid after its document was reparsed",
        ));
    }
    Ok((id, at.node))
}

/// `tidyNodeGetText` of the object's node, for its string cast.
pub(crate) fn text(req: &mut Request, object: PhpObject) -> Result<Option<Vec<u8>>, Thrown> {
    let (id, node) = of(req, object)?;
    Ok(with_doc(req, id, |req, doc| doc.node_text(req, node))?.flatten())
}

/// `zend_string_init`, which `zend_update_property_string` and
/// `add_assoc_string` allocate with, one byte or none included.
fn init(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    PhpString::filled(req, bytes.len(), |out| out.copy_from_slice(bytes))
}

fn string(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    init(req, bytes).map(Owner::into_value)
}

/// `tidy_create_node_object`: a `tidyNode` naming `node`, the document
/// one owner more, and its properties, its children's objects among
/// them, written.
pub(crate) fn create(
    req: &mut Request,
    id: DocId,
    node: ffi::Node,
) -> Result<Owner<PhpObject>, Thrown> {
    let class = lookup_class(req, b"tidyNode")?;
    let class = class.ok_or_else(|| fatal(req, "InternalError: tidyNode is not linked"))?;
    let Some(generation) = generation(req, id) else {
        return Err(fatal(
            req,
            "InternalError: a tidyNode of a document no slot holds",
        ));
    };
    let object = new_object(req, class)?;
    if let Some(obj) = object.native_mut::<Obj>(req) {
        obj.doc = Some(id);
        obj.node = Some(NodeAt { node, generation });
    }
    retain(req, id);
    if let Err(thrown) = properties(req, *object, id, node) {
        object.give_up(req);
        return Err(thrown);
    }
    Ok(object)
}

/// `tidy_add_node_default_properties`.
fn properties(
    req: &mut Request,
    object: PhpObject,
    id: DocId,
    node: ffi::Node,
) -> Result<(), Thrown> {
    let Some(facts) = with_doc(req, id, |req, doc| doc.facts(req, node))? else {
        return Ok(());
    };
    let value = string(req, fixed(&facts.text.unwrap_or_default()))?;
    write_prop(req, &object, b"value", value)?;
    let name = string(req, &facts.name.unwrap_or_default())?;
    write_prop(req, &object, b"name", name)?;
    let id_value = match facts.kind {
        NODE_ROOT | NODE_DOCTYPE | NODE_TEXT | NODE_COMMENT => Value::null(),
        _ => Value::int(facts.id),
    };
    for (field, scalar) in [
        (&b"type"[..], Value::int(facts.kind)),
        (b"line", Value::int(facts.line)),
        (b"column", Value::int(facts.column)),
        (b"proprietary", Value::bool(facts.proprietary)),
        (b"id", id_value),
    ] {
        let scalar = Owner::take(req, scalar);
        write_prop(req, &object, field, scalar)?;
    }
    let attribute = match facts.attributes {
        Some(rows) => {
            let mut array = PhpArray::new(req)?;
            for (name, value) in rows {
                let value = match value {
                    Some(value) => string(req, &value)?,
                    None => PhpString::new(req, b"")?.into_value(),
                };
                array.set_symtable_str(req, &name, value)?;
            }
            array.into_value()
        }
        None => Owner::null(),
    };
    write_prop(req, &object, b"attribute", attribute)?;
    let child = if facts.children.is_empty() {
        Owner::null()
    } else {
        let mut array = PhpArray::new(req)?;
        for child in facts.children {
            let made = match create(req, id, child) {
                Ok(made) => made,
                Err(thrown) => {
                    array.give_up(req);
                    return Err(thrown);
                }
            };
            array.append(req, made.into_value())?;
        }
        array.into_value()
    };
    write_prop(req, &object, b"child", child)
}

/// The node `to` walks to from the object's, as a new `tidyNode`.
fn walk(
    req: &mut Request,
    object: PhpObject,
    to: Step,
) -> Result<Option<Owner<Instance<tidyNode>>>, Thrown> {
    let (id, node) = of(req, object)?;
    match with_doc(req, id, |_, doc| Ok(doc.step(node, to)))?.flatten() {
        Some(next) => create(req, id, next).map(|made| Some(made.into_instance())),
        None => Ok(None),
    }
}

/// `tidyNodeGetType` of the object's node.
fn kind(req: &mut Request, object: PhpObject) -> Result<i64, Thrown> {
    let (id, node) = of(req, object)?;
    Ok(with_doc(req, id, |_, doc| Ok(doc.kind(node)))?.unwrap_or(NODE_ROOT))
}

/// `final class tidyNode`, uncloneable, made by its document alone.
///
/// ```php
/// $t = tidy_parse_string('<p class="a" id=b>x<!-- c --></p>', ['newline' => 'LF']);
/// $p = $t->body()->child[0];
/// var_dump($p->name, $p->type, $p->line, $p->column, $p->proprietary, $p->id === TIDY_TAG_P, $p->attribute, count($p->child), (string) $p, (int) $p);
/// try { $p->name = 'x'; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { clone $p; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { new tidyNode; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "p"
/// int(5)
/// int(1)
/// int(1)
/// bool(false)
/// bool(true)
/// array(2) {
///   ["class"]=>
///   string(1) "a"
///   ["id"]=>
///   string(1) "b"
/// }
/// int(2)
/// string(36) "<p class="a" id="b">x<!-- c --></p>
/// "
/// int(0)
/// Cannot modify readonly property tidyNode::$name
/// Trying to clone an uncloneable object of class tidyNode
/// Call to private tidyNode::__construct() from global scope
/// ```
#[allow(non_camel_case_types)]
#[php_class(final, size = 88, payload = Obj, handlers = NODE)]
pub(crate) struct tidyNode;

#[php_class]
#[allow(non_snake_case)]
impl tidyNode {
    #[php(property, readonly)]
    const value: PhpString;
    #[php(property, readonly)]
    const name: PhpString;
    #[php(property, readonly)]
    const r#type: i64;
    #[php(property, readonly)]
    const line: i64;
    #[php(property, readonly)]
    const column: i64;
    #[php(property, readonly)]
    const proprietary: bool;
    #[php(property, readonly)]
    const id: Option<i64>;
    #[php(property, readonly)]
    const attribute: Option<PhpArray>;
    #[php(property, readonly)]
    const child: Option<PhpArray>;

    /// `private __construct()`.
    ///
    /// ```php
    /// try { (new ReflectionMethod('tidyNode', '__construct'))->invoke(tidy_parse_string('<p>')->body()); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// You should not create a tidyNode manually
    /// ```
    #[php(private)]
    fn __construct(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let _ = this;
        Err(throw_error(
            req,
            "Error",
            "You should not create a tidyNode manually",
        ))
    }

    /// `hasChildren(): bool`.
    ///
    /// ```php
    /// $b = tidy_parse_string('<p>x')->body(); var_dump($b->hasChildren(), $b->child[0]->child[0]->hasChildren());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    fn hasChildren(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (id, node) = of(req, this.object())?;
        Ok(with_doc(req, id, |_, doc| Ok(doc.step(node, Step::Child)))?
            .flatten()
            .is_some())
    }

    /// `hasSiblings(): bool`.
    ///
    /// ```php
    /// $b = tidy_parse_string('<p>x<p>y')->body(); var_dump($b->child[0]->hasSiblings(), $b->child[1]->hasSiblings());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    fn hasSiblings(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (id, node) = of(req, this.object())?;
        Ok(with_doc(req, id, |_, doc| Ok(doc.step(node, Step::Next)))?
            .flatten()
            .is_some())
    }

    /// `isComment(): bool`.
    ///
    /// ```php
    /// $b = tidy_parse_string('<p><!-- c --></p>')->body(); var_dump($b->child[0]->child[0]->isComment(), $b->isComment());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    fn isComment(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        Ok(kind(req, this.object())? == NODE_COMMENT)
    }

    /// `isHtml(): bool`: a start, an end or an empty tag.
    ///
    /// ```php
    /// $b = tidy_parse_string('<p>x</p>')->body(); var_dump($b->isHtml(), $b->child[0]->child[0]->isHtml());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    fn isHtml(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (id, node) = of(req, this.object())?;
        Ok(with_doc(req, id, |_, doc| Ok(doc.is_html(node)))?.unwrap_or(false))
    }

    /// `isText(): bool`.
    ///
    /// ```php
    /// var_dump(tidy_parse_string('<p>x')->body()->child[0]->child[0]->isText());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    fn isText(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        Ok(kind(req, this.object())? == NODE_TEXT)
    }

    /// `isJste(): bool`.
    ///
    /// ```php
    /// var_dump(tidy_parse_string('<# #>')->root()->child[0]->isJste());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    fn isJste(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        Ok(kind(req, this.object())? == NODE_JSTE)
    }

    /// `isAsp(): bool`.
    ///
    /// ```php
    /// var_dump(tidy_parse_string('<% %>')->root()->child[0]->isAsp());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    fn isAsp(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        Ok(kind(req, this.object())? == NODE_ASP)
    }

    /// `isPhp(): bool`.
    ///
    /// ```php
    /// var_dump(tidy_parse_string('<?php echo 1; ?>')->root()->child[0]->isPhp());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    fn isPhp(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        Ok(kind(req, this.object())? == NODE_PHP)
    }

    /// `getParent(): ?tidyNode`.
    ///
    /// ```php
    /// $t = tidy_parse_string('<p>x'); var_dump($t->body()->child[0]->getParent()->name, $t->root()->getParent());
    /// ```
    /// ```expect
    /// string(4) "body"
    /// NULL
    /// ```
    fn getParent(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<tidyNode>>>, Thrown> {
        walk(req, this.object(), Step::Parent)
    }

    /// `getPreviousSibling(): ?tidyNode`.
    ///
    /// ```php
    /// $b = tidy_parse_string('<p>x<p>y')->body(); var_dump($b->child[1]->getPreviousSibling()->name, $b->child[0]->getPreviousSibling());
    /// ```
    /// ```expect
    /// string(1) "p"
    /// NULL
    /// ```
    fn getPreviousSibling(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<tidyNode>>>, Thrown> {
        walk(req, this.object(), Step::Prev)
    }

    /// `getNextSibling(): ?tidyNode`.
    ///
    /// ```php
    /// $b = tidy_parse_string('<p>x<i>y</i>')->body(); var_dump($b->child[0]->getNextSibling()?->name);
    /// ```
    /// ```expect
    /// NULL
    /// ```
    fn getNextSibling(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<tidyNode>>>, Thrown> {
        walk(req, this.object(), Step::Next)
    }
}
