//! `DOMXPath` (`xpath.c`, `xpath_callbacks.c`): a libxml2 `XPath`
//! evaluator over one document, its namespaces, and the php functions an
//! expression may call through `php:function`, `php:functionString` and
//! the names `registerPhpFunctionNS` registered.

use php_ext::abi::{Callable, Instance, Passed, Path, Raw, Refusals, This, Union};
use php_ext::attr::php_class;
use php_ext::call::call;
use php_ext::coerce::{callable_checked, to_string};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, docref, pending, throw_chained, throw_error,
};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi::{
    self, Answer, Called, Item, Name, Node, Operand, Returned, Route, Scope,
};

use crate::document::{DOMDocument, Document};
use crate::map::NodeList;
use crate::modern::Node as ModernNode;
use crate::node::DOMNode;
use crate::object::{Code, Dom, Extra};
use crate::props::Prop;
use crate::{Read, dom, dom_mut, fetch, object_for, string};

/// `PHP_DOM_REG_FUNC_MODE_*`: which php functions one namespace lets an
/// expression call.
#[derive(Default)]
enum Mode {
    /// Nothing registered: a call is php's `Error`.
    #[default]
    None,
    /// `registerPhpFunctions()` with no argument: any function by name.
    All,
    /// The functions registered by name, each with the value it was read
    /// off, one owner held.
    Set(Vec<(Vec<u8>, Callable, Value)>),
}

/// `dom_xpath_object`'s own fields.
pub(crate) struct XPathState {
    ctx: ffi::XPath,
    register_node_ns: bool,
    callbacks: XPathCallbacks,
}

/// `php_dom_xpath_callbacks`, as `xpath_callbacks.h` exports it
/// (`PHP_DOM_EXPORT`) to the extensions that include it, ext/xsl's
/// `XSLTProcessor` keeping one beside its stylesheet (design §4.10): the
/// php functions an expression may call, by namespace, and the values the
/// calls handed out, kept alive as long as their owner.
#[derive(Default)]
pub struct XPathCallbacks {
    /// `php_ns`, the functions of `php:function`.
    php_ns: Mode,
    /// `namespaces`: the functions `registerPhpFunctionNS` registered, by
    /// namespace in registration order.
    namespaces: Vec<(Vec<u8>, Mode)>,
    /// `node_list`: the values a call handed a function or had back, kept
    /// alive as long as the evaluator, one owner each.
    node_list: Vec<Value>,
}

impl XPathCallbacks {
    /// `php_dom_xpath_callbacks_dtor`: every registered function and kept
    /// value let go of.
    pub fn release(self, req: &mut Request) {
        for mode in std::iter::once(self.php_ns).chain(self.namespaces.into_iter().map(|(_, m)| m))
        {
            if let Mode::Set(functions) = mode {
                for (_, _, value) in functions {
                    req.release(value);
                }
            }
        }
        for value in self.node_list {
            req.release(value);
        }
    }

    /// `php_dom_xpath_callbacks_get_gc`: every value the table holds.
    pub fn visit(&self, visit: &mut dyn FnMut(Value)) {
        for mode in std::iter::once(&self.php_ns).chain(self.namespaces.iter().map(|(_, m)| m)) {
            if let Mode::Set(functions) = mode {
                for (_, _, value) in functions {
                    visit(*value);
                }
            }
        }
        for value in &self.node_list {
            visit(*value);
        }
    }

    /// `php_dom_xpath_callbacks_clean_node_list`: the values the calls of
    /// one evaluation handed out let go of.
    pub fn clean_node_list(&mut self, req: &mut Request) {
        for value in std::mem::take(&mut self.node_list) {
            req.release(value);
        }
    }

    /// `php_dom_xpath_callbacks_update_method_handler` with no namespace
    /// and `PHP_DOM_XPATH_CALLBACK_NAME_VALIDATE_NULLS`:
    /// `registerPhpFunctions($restrict)`, every function where `restrict`
    /// is null, and the ones it names otherwise.
    pub fn register_php(
        &mut self,
        req: &mut Request,
        restrict: Option<Value>,
    ) -> Result<(), Thrown> {
        register_into(req, &mut self.php_ns, restrict)
    }

    /// `php_dom_xpath_callbacks_update_single_method_handler` under
    /// `PHP_DOM_XPATH_CALLBACK_NAME_VALIDATE_NCNAME`: `callable`, the third
    /// argument, registered as `name` in `ns`, one owner of it taken.
    pub fn register_ns(
        &mut self,
        req: &mut Request,
        ns: &[u8],
        name: &[u8],
        callable: &Value,
    ) -> Result<(), Thrown> {
        if name.is_empty() || !Name::valid_ncname(name) {
            return Err(arg_value_error(req, 2, "be a valid callback name"));
        }
        let (callable, value) = callable_of(req, callable, 3, false)?;
        let at = if let Some(at) = self.namespaces.iter().position(|(n, _)| n == ns) {
            at
        } else {
            self.namespaces.push((ns.to_vec(), Mode::None));
            self.namespaces.len() - 1
        };
        let mut functions = match std::mem::take(&mut self.namespaces[at].1) {
            Mode::Set(f) => f,
            _ => Vec::new(),
        };
        if let Some(slot) = functions.iter_mut().find(|(n, _, _)| *n == name) {
            let old = std::mem::replace(&mut slot.2, value);
            slot.1 = callable;
            req.release(old);
        } else {
            functions.push((name.to_vec(), callable, value));
        }
        self.namespaces[at].1 = Mode::Set(functions);
        Ok(())
    }

    /// `php_dom_xpath_callbacks_delayed_lib_registration`'s walk: each
    /// namespace and name `registerPhpFunctionNS` registered, which the
    /// owner registers on an evaluator of its own.
    #[must_use]
    pub fn registered(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        for (ns, mode) in &self.namespaces {
            if let Mode::Set(functions) = mode {
                out.extend(functions.iter().map(|(n, _, _)| (ns.clone(), n.clone())));
            }
        }
        out
    }

    /// `php_dom_xpath_callbacks_call_php_ns` and
    /// `php_dom_xpath_callbacks_call_custom_ns`: the function `called`
    /// names served with `operands`, each node handed to it made an
    /// object by `proxy`, php's `proxy_factory`; `modern` is whether the
    /// document follows the spec, which decides the node class an answer
    /// is held to.
    pub fn call(
        &mut self,
        req: &mut Request,
        called: Called,
        operands: Vec<Operand>,
        modern: bool,
        proxy: &mut Proxy<'_>,
    ) -> Result<Returned, Thrown> {
        dispatch(req, self, called, operands, modern, proxy)
    }
}

/// `php_dom_xpath_callbacks_proxy_factory`: the object a node handed to a
/// php function stands as.
pub type Proxy<'a> = dyn FnMut(&mut Request, Node) -> Result<Owner<Value>, Thrown> + 'a;

/// The evaluator of `this`, taken out of the payload for one call.
fn take(req: &mut Request, this: PhpObject) -> Option<Box<XPathState>> {
    let d = dom_mut(req, this)?;
    match std::mem::take(&mut d.extra) {
        Extra::XPath(state) => Some(state),
        other => {
            d.extra = other;
            None
        }
    }
}

fn put(req: &mut Request, this: PhpObject, state: Box<XPathState>) {
    if let Some(d) = dom_mut(req, this) {
        d.extra = Extra::XPath(state);
    }
}

/// `dom_xpath_document_read`.
fn document_read(req: &mut Request, object: PhpObject) -> Read {
    let doc = match dom(req, object) {
        Some(d) => match &d.extra {
            Extra::XPath(state) => state.ctx.doc(),
            _ => None,
        },
        None => None,
    };
    match doc {
        Some(doc) => Ok(Some(object_for(req, doc, Some(object))?)),
        None => Err(crate::dom_throw(req, Code::InvalidState)),
    }
}

#[allow(clippy::unnecessary_wraps)] // the reader shape every row of a property table has
fn register_node_ns_read(req: &mut Request, object: PhpObject) -> Read {
    // `dom_xpath_objects_new` sets it before any constructor runs.
    let on = match dom(req, object).map(|d| &d.extra) {
        Some(Extra::XPath(s)) => s.register_node_ns,
        _ => true,
    };
    Ok(Some(Owner::take(req, Value::bool(on))))
}

#[allow(clippy::unnecessary_wraps)] // the writer shape every row of a property table has
fn register_node_ns_write(
    req: &mut Request,
    object: PhpObject,
    value: &Value,
) -> Result<(), Thrown> {
    let on = value.as_bool().unwrap_or(false);
    if let Some(d) = dom_mut(req, object)
        && let Extra::XPath(state) = &mut d.extra
    {
        state.register_node_ns = on;
    }
    Ok(())
}

/// `dom_xpath_prop_handlers`.
pub(crate) const PROPS: &[Prop] = &[
    Prop {
        name: b"document",
        read: document_read,
        write: None,
    },
    Prop {
        name: b"registerNodeNamespaces",
        read: register_node_ns_read,
        write: Some(register_node_ns_write),
    },
];

/// The object an item of a node set answers as: a node's, or a namespace
/// node standing for the declaration, its element kept alive by it.
fn item_object(
    req: &mut Request,
    proxy: &mut Proxy<'_>,
    item: Item,
) -> Result<Owner<Value>, Thrown> {
    match item {
        Item::Node(node) => proxy(req, node),
        Item::Namespace {
            href,
            prefix,
            parent,
        } => {
            let Some(parent) = parent else {
                return Ok(Owner::take(req, Value::null()));
            };
            let parent_value = proxy(req, parent)?;
            let Some(parent_obj) = parent_value.as_object().copied() else {
                parent_value.give_up(req);
                return Ok(Owner::take(req, Value::null()));
            };
            let Some(fake) = Node::fake_namespace_decl(parent, href.as_deref(), prefix.as_deref())
            else {
                parent_value.give_up(req);
                return Err(crate::dom_throw(req, Code::InvalidState));
            };
            let value = match object_for(req, fake, Some(parent_obj)) {
                Ok(value) => value,
                Err(thrown) => {
                    parent_value.give_up(req);
                    return Err(thrown);
                }
            };
            if let Some(object) = value.as_object().copied()
                && let Some(d) = dom_mut(req, object)
            {
                d.extra = Extra::NamespaceParent(parent_obj);
                // The parent's owner is the namespace node's from here.
                let _parent = parent_value.hand_on_unmigrated();
            } else {
                parent_value.give_up(req);
            }
            Ok(value)
        }
    }
}

/// `php_dom_xpath_callback_fetch_args`' conversion of one operand.
fn operand_value(
    req: &mut Request,
    proxy: &mut Proxy<'_>,
    operand: Operand,
) -> Result<Owner<Value>, Thrown> {
    match operand {
        Operand::Str(s) => string(req, &s),
        Operand::Bool(b) => Ok(Owner::take(req, Value::bool(b))),
        Operand::Number(n) => Ok(Owner::take(req, Value::float(n))),
        // `ZVAL_EMPTY_ARRAY` for a node set with no node
        // (`Z/ext/dom/xpath_callbacks.c:367`).
        Operand::Nodes(items) if items.is_empty() => {
            Ok(Owner::take(req, Value::array(PhpArray::empty())))
        }
        Operand::Nodes(items) => {
            let mut array = PhpArray::new(req)?;
            for item in items {
                let v = match item_object(req, proxy, item) {
                    Ok(v) => v,
                    Err(thrown) => {
                        array.give_up(req);
                        return Err(thrown);
                    }
                };
                array.append(req, v.hand_on_unmigrated())?;
            }
            Ok(array.into_value())
        }
    }
}

/// The bytes of `value`'s string form, the string given up.
fn string_bytes(req: &mut Request, value: &Value) -> Result<Vec<u8>, Thrown> {
    let text = to_string(req, value)?;
    let bytes = text.as_bytes().to_vec();
    text.give_up(req);
    Ok(bytes)
}

/// Each operand converted, those converted given up where one throws.
fn operand_values(
    req: &mut Request,
    proxy: &mut Proxy<'_>,
    operands: Vec<Operand>,
) -> Result<Vec<Owner<Value>>, Thrown> {
    let mut args: Vec<Owner<Value>> = Vec::with_capacity(operands.len());
    for operand in operands {
        match operand_value(req, proxy, operand) {
            Ok(arg) => args.push(arg),
            Err(thrown) => {
                for a in args {
                    a.give_up(req);
                }
                return Err(thrown);
            }
        }
    }
    Ok(args)
}

/// `php_dom_xpath_callbacks_call_php_ns`'s refusal of a name that is no
/// string.
const NOT_A_NAME: &str = "Handler name must be a string";

/// `php_dom_xpath_callback_dispatch`.
#[allow(clippy::too_many_lines)] // php's function, each owner given up on the path that leaves it
fn dispatch(
    req: &mut Request,
    state: &mut XPathCallbacks,
    called: Called,
    operands: Vec<Operand>,
    modern: bool,
    proxy: &mut Proxy<'_>,
) -> Result<Returned, Thrown> {
    let (mode_ns, name, operands) = match called {
        Called::Php { .. } => {
            let mut operands = operands.into_iter();
            let Some(first) = operands.next() else {
                return Err(throw_error(
                    req,
                    "Error",
                    "Function name must be passed as the first argument",
                ));
            };
            let Operand::Str(name) = first else {
                return Err(throw_error(req, "TypeError", NOT_A_NAME));
            };
            (None, name, operands.collect::<Vec<_>>())
        }
        Called::Custom { ns, name } => (Some(ns), name, operands),
    };
    let args = operand_values(req, proxy, operands)?;
    let mode = match &mode_ns {
        None => Some(&state.php_ns),
        Some(ns) => state
            .namespaces
            .iter()
            .find(|(n, _)| n == ns)
            .map(|(_, m)| m),
    };
    let callable = match mode {
        None | Some(Mode::None) => Err(throw_error(req, "Error", "No callbacks were registered")),
        Some(Mode::All) => {
            let name_value = string(req, &name)?;
            let found = callable_checked(req, &name_value);
            name_value.give_up(req);
            found.map_err(|why| {
                let shown = String::from_utf8_lossy(&name).into_owned();
                let message = format!("Invalid callback {shown}, {why}");
                // An autoloader's throw is the refusal's previous, as
                // `zend_throw_error` chains onto `EG(exception)`.
                match pending(req) {
                    Err(previous) => throw_chained(req, previous, "Error", &message),
                    Ok(()) => throw_error(req, "Error", &message),
                }
            })
        }
        Some(Mode::Set(functions)) => {
            if let Some((_, c, _)) = functions.iter().find(|(n, _, _)| *n == name) {
                Ok(c.clone())
            } else {
                let shown = String::from_utf8_lossy(&name).into_owned();
                Err(throw_error(
                    req,
                    "Error",
                    &format!("No callback handler \"{shown}\" registered"),
                ))
            }
        }
    };
    let callable = match callable {
        Ok(c) => c,
        Err(thrown) => {
            for a in args {
                a.give_up(req);
            }
            return Err(thrown);
        }
    };
    let views: Vec<Value> = args.iter().map(|a| **a).collect();
    let answered = call(req, &callable, &views);
    for a in args {
        if a.as_object().is_some() || a.as_array().is_some() {
            state.node_list.push(a.hand_on_unmigrated());
        } else {
            a.give_up(req);
        }
    }
    let answer = answered?;
    let node_class = if modern { "Dom\\Node" } else { "DOMNode" };
    let returned = match answer.as_ref() {
        ValueRef::Object(o) if crate::instance_of(req, *o, node_class) => {
            match crate::node_of(req, *o) {
                Some(node) => {
                    state.node_list.push(answer.hand_on_unmigrated());
                    return Ok(Returned::Node(node));
                }
                None => Ok(Returned::Sentinel),
            }
        }
        ValueRef::Bool(b) => Ok(Returned::Bool(b)),
        ValueRef::Object(_) => {
            answer.give_up(req);
            return Err(throw_error(
                req,
                "TypeError",
                "Only objects that are instances of DOM nodes can be converted to an XPath expression",
            ));
        }
        _ => string_bytes(req, &answer).map(Returned::Str),
    };
    answer.give_up(req);
    returned
}

/// `php_xpath_eval`: `query` forces a node set, `evaluate` answers what
/// the expression does.
fn eval(
    req: &mut Request,
    this: PhpObject,
    expr: &[u8],
    context: Option<PhpObject>,
    register: Passed<bool>,
    force_node_set: bool,
) -> Result<Owner<Value>, Thrown> {
    let register = match register {
        Passed::Given(on) => on,
        Passed::Omitted => {
            matches!(dom(req, this), Some(d) if matches!(&d.extra, Extra::XPath(s) if s.register_node_ns))
        }
    };
    let modern = crate::modern::follows_spec(req, this);
    let Some(mut state) = take(req, this) else {
        return Err(throw_error(req, "Error", "Invalid XPath Context"));
    };
    let Some(doc) = state.ctx.doc() else {
        put(req, this, state);
        if modern {
            return Err(throw_error(req, "Error", "Invalid XPath Document Pointer"));
        }
        docref(req, Level::Warning, "Invalid XPath Document Pointer");
        return Ok(Owner::take(req, Value::bool(false)));
    };
    let node = match context {
        Some(o) => match fetch(req, o) {
            Ok(n) => Some(n),
            Err(thrown) => {
                put(req, this, state);
                return Err(thrown);
            }
        },
        None => doc.root(),
    };
    if node.is_some_and(|n| n.doc() != Some(doc)) {
        put(req, this, state);
        return Err(throw_error(req, "Error", "Node from wrong document"));
    }
    let in_scope = match node {
        Some(n) if register && modern => dom_mut(req, this)
            .and_then(|d| crate::private::of(&mut d.obj))
            .map(|p| p.mapper.in_scope(n, false))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let scope = match (register, modern) {
        (false, _) => Scope::None,
        (true, false) => Scope::InScope,
        (true, true) => Scope::List(&in_scope),
    };
    let (answer, raised, thrown) = {
        let XPathState { ctx, callbacks, .. } = &mut *state;
        let mut proxy = |req: &mut Request, node: Node| object_for(req, node, Some(this));
        let mut handler = |req: &mut Request, called: Called, operands: Vec<Operand>| {
            dispatch(req, callbacks, called, operands, modern, &mut proxy)
        };
        ctx.eval_scoped(req, expr, node, scope, &mut handler)
    };
    put(req, this, state);
    let mut raised = raised;
    for error in &mut raised {
        error.route = Route::Generic;
    }
    Errors::route(req, raised);
    if let Some(thrown) = thrown {
        return Err(thrown);
    }
    let Some(answer) = answer else {
        if modern {
            return Err(throw_error(
                req,
                "Error",
                "Could not evaluate XPath expression",
            ));
        }
        return Ok(Owner::take(req, Value::bool(false)));
    };
    let answer = if force_node_set && !matches!(answer, Answer::NodeSet(_)) {
        Answer::NodeSet(Vec::new())
    } else {
        answer
    };
    match answer {
        Answer::NodeSet(items) => node_list(req, this, items, modern),
        Answer::Bool(b) => Ok(Owner::take(req, Value::bool(b))),
        Answer::Number(n) => Ok(Owner::take(req, Value::float(n))),
        Answer::Str(s) => string(req, &s),
        Answer::Other => Ok(Owner::take(req, Value::null())),
    }
}

/// The node list `php_xpath_eval` hands back for a node set: one object
/// per node, and the process's empty array under the list for a set with
/// no node (`ZVAL_EMPTY_ARRAY`, `Z/ext/dom/xpath.c:385`). A modern
/// document refuses the namespace axis.
fn node_list(
    req: &mut Request,
    this: PhpObject,
    items: Vec<Item>,
    modern: bool,
) -> Result<Owner<Value>, Thrown> {
    let mut array = if items.is_empty() {
        Owner::take(req, PhpArray::empty())
    } else {
        PhpArray::new(req)?
    };
    for item in items {
        if modern && matches!(item, Item::Namespace { .. }) {
            array.give_up(req);
            return Err(crate::dom_throw_message(
                req,
                Code::NotSupported,
                "The namespace axis is not well-defined in the living DOM specification. \
                 Use Dom\\Element::getInScopeNamespaces() or Dom\\Element::getDescendantNamespaces() instead.",
            ));
        }
        let v = match item_object(
            req,
            &mut |req, node| object_for(req, node, Some(this)),
            item,
        ) {
            Ok(v) => v,
            Err(thrown) => {
                array.give_up(req);
                return Err(thrown);
            }
        };
        array.append(req, v.hand_on_unmigrated())?;
    }
    let class = if modern {
        "Dom\\NodeList"
    } else {
        "DOMNodeList"
    };
    crate::node_set(req, class, array)
}

/// One callable the registration read, with the value it came from, one
/// owner taken.
fn callable_of(
    req: &mut Request,
    value: &Value,
    position: u32,
    is_array: bool,
) -> Result<(Callable, Value), Thrown> {
    match callable_checked(req, value) {
        Ok(c) => {
            req.addref(value);
            Ok((c, *value))
        }
        Err(why) => {
            let tail = if is_array {
                format!("be an array with valid callbacks as values, {why}")
            } else {
                format!("be a callable, {why}")
            };
            Err(arg_type_error(req, position, &tail))
        }
    }
}

/// The names and callbacks `restrict` registers, pushed onto `adding`
/// one by one, each callback's value an owner [`callable_of`] took; on
/// the `Err` those already pushed are the caller's to give up.
fn collect(
    req: &mut Request,
    restrict: Value,
    adding: &mut Vec<(Vec<u8>, (Callable, Value))>,
) -> Result<(), Thrown> {
    if let Some(array) = restrict.as_array().copied() {
        for (key, value) in &array {
            let name = match key {
                Key::Str(s) => s.as_bytes().to_vec(),
                Key::Int(_) => string_bytes(req, &value)?,
            };
            if name.is_empty() || name.contains(&0) {
                return Err(arg_value_error(
                    req,
                    1,
                    "be an array containing valid callback names",
                ));
            }
            adding.push((name, callable_of(req, &value, 1, true)?));
        }
    } else {
        let name = restrict
            .as_str_bytes()
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        if name.is_empty() || name.contains(&0) {
            return Err(arg_value_error(req, 1, "be a valid callback name"));
        }
        adding.push((name, callable_of(req, &restrict, 1, false)?));
    }
    Ok(())
}

/// `php_dom_xpath_callback_ns_update_method_handler`.
fn register_into(
    req: &mut Request,
    mode: &mut Mode,
    restrict: Option<Value>,
) -> Result<(), Thrown> {
    let restrict = restrict
        .map(|v| v.deref())
        .filter(|v| !matches!(v.as_ref(), ValueRef::Null));
    let Some(restrict) = restrict else {
        *mode = Mode::All;
        return Ok(());
    };
    let mut adding = Vec::new();
    if let Err(thrown) = collect(req, restrict, &mut adding) {
        for (_, (_, value)) in adding {
            req.release(value);
        }
        return Err(thrown);
    }
    let mut functions = match std::mem::take(mode) {
        Mode::Set(f) => f,
        _ => Vec::new(),
    };
    for (name, (callable, value)) in adding {
        if let Some(slot) = functions.iter_mut().find(|(n, _, _)| *n == name) {
            let old = std::mem::replace(&mut slot.2, value);
            slot.1 = callable;
            req.release(old);
        } else {
            functions.push((name, callable, value));
        }
    }
    *mode = Mode::Set(functions);
    Ok(())
}

/// `class DOMXPath`, `@not-serializable`: an evaluator over one
/// document, its `document` and `registerNodeNamespaces` `@virtual`,
/// served by [`HANDLERS`].
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a>1</a><a>2</a></r>');
/// $x = new DOMXPath($d);
/// var_dump($x->document === $d, $x->registerNodeNamespaces, $x->query('//a')->length, $x->query('//a')->item(1)->textContent);
/// $root = $d->documentElement;
/// var_dump($x->query('a', $root)->length, $x->evaluate('count(//a)'), $x->evaluate('string(//a)'), $x->evaluate('1 = 1'));
/// var_dump($x->query('//['));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// int(2)
/// string(1) "2"
/// int(2)
/// float(2)
/// string(1) "1"
/// bool(true)
///
/// Warning: DOMXPath::query(): Invalid expression in %s on line %d
/// bool(false)
/// ```
#[php_class(not_serializable, size = 112, payload = Dom, handlers = HANDLERS)]
pub(crate) struct DOMXPath;

#[php_class]
impl DOMXPath {
    #[php(property, virtual)]
    const document: Instance<DOMDocument>;
    #[php(property, virtual)]
    const registerNodeNamespaces: bool;

    /// `DOMXPath::__construct(DOMDocument $document, bool $registerNodeNS = true)`.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        document: Instance<DOMDocument>,
        #[php(default = true)] registerNodeNS: bool,
    ) -> Result<(), Thrown> {
        let (this, doc_obj) = (this.object(), document.object());
        let doc = fetch(req, doc_obj)?;
        let Some(ctx) = ffi::XPath::new(doc) else {
            return Err(crate::dom_throw(req, Code::InvalidState));
        };
        if let Some(old) = take(req, this) {
            old.callbacks.release(req);
        }
        let shared = crate::shared_doc(req, doc_obj);
        if let Some(mut obj) = crate::libxml_half(req, this) {
            obj.decrement_doc_ref(req);
            obj.share(req, shared, doc);
        }
        let state = XPathState {
            ctx,
            register_node_ns: registerNodeNS,
            callbacks: XPathCallbacks::default(),
        };
        put(req, this, Box::new(state));
        Ok(())
    }

    /// `DOMXPath::evaluate(string $expression, ?DOMNode $contextNode = null, bool $registerNodeNS = true): mixed`;
    /// an omitted `$registerNodeNS` is the evaluator's `registerNodeNamespaces`.
    #[php(tentative)]
    fn evaluate(
        req: &mut Request,
        this: This<Self>,
        expression: PhpString,
        #[php(default = null)] contextNode: Option<Instance<DOMNode>>,
        #[php(default = true)] registerNodeNS: Passed<bool>,
    ) -> Result<Owner<Value>, Thrown> {
        let context = contextNode.map(|c| c.object());
        eval(
            req,
            this.object(),
            expression.as_bytes(),
            context,
            registerNodeNS,
            false,
        )
    }

    /// `DOMXPath::query(string $expression, ?DOMNode $contextNode = null, bool $registerNodeNS = true): mixed`,
    /// and `Dom\XPath::query`, whose failures throw and whose document's
    /// in-scope declarations come from its `xmlns` attributes.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p"><p:a>1</p:a><p:a>2</p:a></r>');
    /// $x = new Dom\XPath($d);
    /// $l = $x->query('//p:a');
    /// var_dump(get_class($l), $l->length, $x->evaluate('string(//p:a[2])'), $x->document === $d);
    /// try { $x->query('//['); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// try { $x->query('//namespace::*'); } catch (DOMException $e) { echo $e->code, "\n"; }
    /// ```
    /// ```expect
    /// string(12) "Dom\NodeList"
    /// int(2)
    /// string(1) "2"
    /// bool(true)
    ///
    /// Warning: Dom\XPath::query(): Invalid expression in %s on line %d
    /// Error: Could not evaluate XPath expression
    /// 9
    /// ```
    ///
    /// A node set with no node is the process's empty array, under the
    /// list and as a callback's argument.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<a><b/></a>');
    /// $x = new DOMXPath($d);
    /// $l = $x->query('//c');
    /// $m = memory_get_usage();
    /// $k = $x->query('//c');
    /// var_dump(memory_get_usage() - $m, $k->length);
    /// $x->registerNamespace('php', 'http://php.net/xpath');
    /// $x->registerPhpFunctions();
    /// function f($nodes) { debug_zval_dump($nodes); return 1; }
    /// $x->evaluate('php:function("f", //c)');
    /// ```
    /// ```expect
    /// int(160)
    /// int(0)
    /// array(0) interned {
    /// }
    /// ```
    #[php(tentative)]
    fn query(
        req: &mut Request,
        this: This<Self>,
        expression: PhpString,
        #[php(default = null)] contextNode: Option<Instance<DOMNode>>,
        #[php(default = true)] registerNodeNS: Passed<bool>,
    ) -> Result<Owner<Value>, Thrown> {
        let context = contextNode.map(|c| c.object());
        eval(
            req,
            this.object(),
            expression.as_bytes(),
            context,
            registerNodeNS,
            true,
        )
    }

    /// `DOMXPath::registerNamespace(string $prefix, string $namespace): bool`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r xmlns="urn:d"><c/></r>');
    /// $x = new DOMXPath($d);
    /// var_dump($x->query('//c')->length, $x->registerNamespace('d', 'urn:d'), $x->query('//d:c')->length);
    /// ```
    /// ```expect
    /// int(0)
    /// bool(true)
    /// int(1)
    /// ```
    #[php(tentative)]
    fn registerNamespace(
        req: &mut Request,
        this: This<Self>,
        prefix: PhpString,
        namespace: PhpString,
    ) -> Result<bool, Thrown> {
        let took = match dom(req, this.object()) {
            Some(d) => match &d.extra {
                Extra::XPath(state) => Some(
                    state
                        .ctx
                        .register_ns(prefix.as_bytes(), namespace.as_bytes()),
                ),
                _ => None,
            },
            None => None,
        };
        match took {
            Some(ok) => Ok(ok),
            None => Err(throw_error(req, "Error", "Invalid XPath Context")),
        }
    }

    /// `DOMXPath::registerPhpFunctions(string|array|null $restrict = null): void`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r><a>x</a></r>');
    /// $x = new DOMXPath($d);
    /// $x->registerNamespace('php', 'http://php.net/xpath');
    /// $x->registerPhpFunctions('strtoupper');
    /// var_dump($x->evaluate('php:functionString("strtoupper", //a)'));
    /// $x->registerPhpFunctions(['up' => fn($s) => strtoupper($s) . '!']);
    /// var_dump($x->evaluate('php:functionString("up", //a)'));
    /// try { $x->evaluate('php:functionString("strrev", //a)'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $x->registerPhpFunctions(function () {}); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "X"
    /// string(2) "X!"
    /// No callback handler "strrev" registered
    /// DOMXPath::registerPhpFunctions(): Argument #1 ($restrict) must be of type array|string|null, Closure given
    /// ```
    #[php(tentative)]
    fn registerPhpFunctions(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] restrict: Option<Union<PhpString, PhpArray>>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let Some(mut state) = take(req, this) else {
            return Err(throw_error(req, "Error", "Invalid XPath Context"));
        };
        let restrict = restrict.map(|given| match given {
            Union::A(name) => Value::string(name),
            Union::B(array) => Value::array(array),
        });
        let answered = register_into(req, &mut state.callbacks.php_ns, restrict);
        put(req, this, state);
        answered
    }

    /// `DOMXPath::registerPhpFunctionNS(string $namespaceURI, string $name, callable $callable): void`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r><a>x</a></r>');
    /// $x = new DOMXPath($d);
    /// $x->registerNamespace('my', 'urn:my');
    /// $x->registerPhpFunctionNS('urn:my', 'twice', fn($s) => $s . $s);
    /// var_dump($x->evaluate('my:twice(string(//a))'));
    /// try { $x->registerPhpFunctionNS('http://php.net/xpath', 'f', 'strlen'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(2) "xx"
    /// DOMXPath::registerPhpFunctionNS(): Argument #1 ($namespaceURI) must not be "http://php.net/xpath" because it is reserved by PHP
    /// ```
    fn registerPhpFunctionNS(
        req: &mut Request,
        this: This<Self>,
        namespaceURI: Path,
        name: Path,
        callable: Raw<Callable>,
    ) -> Result<(), Thrown> {
        // Held to `callable` before anything else is read, as ZPP holds
        // it; the value is what the registration keeps.
        callable.hold(req)?;
        let this = this.object();
        let (ns, name) = (
            namespaceURI.0.as_bytes().to_vec(),
            name.0.as_bytes().to_vec(),
        );
        if ns == b"http://php.net/xpath" {
            return Err(arg_value_error(
                req,
                1,
                "not be \"http://php.net/xpath\" because it is reserved by PHP",
            ));
        }
        let Some(mut state) = take(req, this) else {
            return Err(throw_error(req, "Error", "Invalid XPath Context"));
        };
        let registered = state
            .callbacks
            .register_ns(req, &ns, &name, &callable.value());
        if registered.is_ok() {
            state.ctx.register_custom(&ns, &name);
        }
        put(req, this, state);
        registered
    }

    /// `DOMXPath::quote(string $str): string`.
    ///
    /// ```php
    /// echo DOMXPath::quote("a"), ' ', DOMXPath::quote("it's"), ' ', DOMXPath::quote("'\"\""), "\n";
    /// ```
    /// ```expect
    /// 'a' "it's" concat("'",'""')
    /// ```
    fn quote(req: &mut Request, str: Path) -> Result<Owner<PhpString>, Thrown> {
        let input = str.0.as_bytes();
        if !input.contains(&b'\'') {
            return PhpString::new(req, &[b"'".as_slice(), input, b"'"].concat());
        }
        if !input.contains(&b'"') {
            return PhpString::new(req, &[b"\"".as_slice(), input, b"\""].concat());
        }
        let mut out = b"concat(".to_vec();
        let mut at = 0;
        while at < input.len() {
            let rest = &input[at..];
            let to_single = rest.iter().position(|b| *b == b'\'').unwrap_or(rest.len());
            let to_double = rest.iter().position(|b| *b == b'"').unwrap_or(rest.len());
            let take = to_single.max(to_double);
            let q = if to_single > to_double { b'\'' } else { b'"' };
            out.push(q);
            out.extend_from_slice(&rest[..take]);
            out.push(q);
            out.push(b',');
            at += take;
        }
        if let Some(last) = out.last_mut() {
            *last = b')';
        }
        PhpString::new(req, &out)
    }
}

/// `final class Dom\XPath`, `@not-serializable`: `DOMXPath`'s bodies
/// over the modern document, its failures thrown.
///
/// `dom_xpath_objects_new` allocates a `dom_xpath_object`, which
/// `memory_get_usage()` counts (design §5.13):
///
/// ```php
/// $doc = Dom\HTMLDocument::createEmpty();
/// new Dom\XPath($doc);
/// $m = memory_get_usage();
/// $x = new Dom\XPath($doc);
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 96
/// ```
#[php_class(
    namespace(Dom),
    final,
    not_serializable,
    size = 112, payload = Dom,
    handlers = HANDLERS
)]
pub(crate) struct XPath;

/// `Dom\XPath::evaluate`'s answer: `null|bool|float|string|Dom\NodeList`.
type Evaluated = Option<Union<Instance<NodeList>, Union<PhpString, Union<f64, bool>>>>;

#[php_class]
impl XPath {
    #[php(property, virtual)]
    const document: Instance<Document>;
    #[php(property, virtual)]
    const registerNodeNamespaces: bool;
    #[php(implementation = DOMXPath::__construct)]
    fn __construct(
        this: This<Self>,
        document: Instance<Document>,
        #[php(default = true)] registerNodeNS: bool,
    );
    #[php(implementation = DOMXPath::evaluate)]
    fn evaluate(
        this: This<Self>,
        expression: PhpString,
        #[php(default = null)] contextNode: Option<Instance<ModernNode>>,
        #[php(default = true)] registerNodeNS: Passed<bool>,
    ) -> Evaluated;
    #[php(implementation = DOMXPath::query)]
    fn query(
        this: This<Self>,
        expression: PhpString,
        #[php(default = null)] contextNode: Option<Instance<ModernNode>>,
        #[php(default = true)] registerNodeNS: Passed<bool>,
    ) -> Instance<NodeList>;
    #[php(implementation = DOMXPath::registerNamespace)]
    fn registerNamespace(this: This<Self>, prefix: PhpString, namespace: PhpString) -> bool;
    #[php(implementation = DOMXPath::registerPhpFunctions)]
    fn registerPhpFunctions(
        this: This<Self>,
        #[php(default = null)] restrict: Option<Union<PhpString, PhpArray>>,
    );
    #[php(implementation = DOMXPath::registerPhpFunctionNS)]
    fn registerPhpFunctionNS(
        this: This<Self>,
        namespaceURI: Path,
        name: Path,
        callable: Raw<Callable>,
    );
    #[php(implementation = DOMXPath::quote)]
    fn quote(str: Path) -> PhpString;
}

/// `dom_xpath_object_handlers`: the dom table with no clone, and the
/// evaluator's registrations given back when the object goes.
pub(crate) struct XPathHandlers;

pub(crate) static HANDLERS: XPathHandlers = XPathHandlers;

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl php_ext::abi::ObjectHandlers for XPathHandlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: php_ext::abi::PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        crate::read_property(req, object, name, read)
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<php_ext::values::PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        crate::write_property(req, object, name, value)
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<php_ext::abi::PropertyPlace, Thrown> {
        crate::property_place(req, object, name)
    }

    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: php_ext::abi::PropertyTest,
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

    /// `dom_xpath_objects_free_storage`: `xmlXPathFreeContext`, then the
    /// document let go of.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        if let Some(state) = take(req, object) {
            state.callbacks.release(req);
        }
        crate::release_libxml(req, object);
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(d) = dom(req, object)
            && let Extra::XPath(state) = &d.extra
        {
            state.callbacks.visit(visit);
        }
    }
}
