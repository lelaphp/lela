//! `DOMXPath` (`xpath.c`, `xpath_callbacks.c`): a libxml2 `XPath`
//! evaluator over one document, its namespaces, and the php functions an
//! expression may call through `php:function`, `php:functionString` and
//! the names `registerPhpFunctionNS` registered.

use php_ext::abi::Callable;
use php_ext::call::call;
use php_ext::coerce::{callable_checked, to_string};
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, Value, ValueRef};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi::{Answer, Called, Item, Name, Node, Operand, Returned, Route, XPath};

use crate::object::{Code, Extra};
use crate::props::Prop;
use crate::{
    Entry, Read, arity, bool_arg, bytes_arg, dom, dom_mut, fetch, object_arg, object_for, string,
    this,
};

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
    ctx: XPath,
    register_node_ns: bool,
    callbacks: Callbacks,
}

/// `php_dom_xpath_callbacks`.
#[derive(Default)]
struct Callbacks {
    /// `php_ns`, the functions of `php:function`.
    php_ns: Mode,
    /// `namespaces`: the functions `registerPhpFunctionNS` registered, by
    /// namespace in registration order.
    namespaces: Vec<(Vec<u8>, Mode)>,
    /// `node_list`: the values a call handed a function or had back, kept
    /// alive as long as the evaluator, one owner each.
    node_list: Vec<Value>,
}

impl Callbacks {
    fn release(self, req: &mut Request) {
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

    fn visit(&self, visit: &mut dyn FnMut(Value)) {
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
}

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

/// `DOMXPath::__construct(DOMDocument $document, bool $registerNodeNS = true)`.
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
fn construct(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 2)?;
    let Some(doc_obj) = object_arg(req, 1)? else {
        return Ok(Value::null());
    };
    let register = bool_arg(req, 2, true)?;
    let this = this(req)?;
    let doc = fetch(req, doc_obj)?;
    let Some(ctx) = XPath::new(doc) else {
        return Err(crate::dom_throw(req, Code::InvalidState));
    };
    if let Some(old) = take(req, this) {
        old.callbacks.release(req);
    }
    let shared = crate::shared_doc(req, doc_obj);
    if let Some(d) = dom_mut(req, this) {
        d.obj.set_object(this);
        d.obj.decrement_doc_ref();
        d.obj.share(shared, doc);
    }
    let state = XPathState {
        ctx,
        register_node_ns: register,
        callbacks: Callbacks::default(),
    };
    put(req, this, Box::new(state));
    Ok(Value::null())
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
    Ok(Some(Value::bool(on)))
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
fn item_object(req: &mut Request, this: PhpObject, item: Item) -> Result<Value, Thrown> {
    match item {
        Item::Node(node) => object_for(req, node, Some(this)),
        Item::Namespace {
            href,
            prefix,
            parent,
        } => {
            let Some(parent) = parent else {
                return Ok(Value::null());
            };
            let parent_value = object_for(req, parent, Some(this))?;
            let Some(parent_obj) = parent_value.as_object().copied() else {
                return Ok(Value::null());
            };
            let Some(fake) = Node::fake_namespace_decl(parent, href.as_deref(), prefix.as_deref())
            else {
                req.release(parent_value);
                return Err(crate::dom_throw(req, Code::InvalidState));
            };
            let value = object_for(req, fake, Some(parent_obj))?;
            if let Some(object) = value.as_object().copied()
                && let Some(d) = dom_mut(req, object)
            {
                d.extra = Extra::NamespaceParent(parent_obj);
            }
            Ok(value)
        }
    }
}

/// `php_dom_xpath_callback_fetch_args`' conversion of one operand.
fn operand_value(req: &mut Request, this: PhpObject, operand: Operand) -> Result<Value, Thrown> {
    match operand {
        Operand::Str(s) => string(req, &s),
        Operand::Bool(b) => Ok(Value::bool(b)),
        Operand::Number(n) => Ok(Value::float(n)),
        Operand::Nodes(items) => {
            let mut array = PhpArray::new(req)?;
            for item in items {
                let v = item_object(req, this, item)?;
                array.append(req, v)?;
            }
            Ok(Value::array(array))
        }
    }
}

/// `php_dom_xpath_callback_dispatch`.
fn dispatch(
    req: &mut Request,
    this: PhpObject,
    state: &mut Callbacks,
    called: Called,
    operands: Vec<Operand>,
) -> Result<Returned, Thrown> {
    let (mode_ns, name, operands) = match called {
        Called::Php { .. } => {
            let mut operands = operands.into_iter();
            let Some(first) = operands.next() else {
                return Err(throw(
                    req,
                    "Error",
                    "Function name must be passed as the first argument",
                ));
            };
            let Operand::Str(name) = first else {
                return Err(throw(req, "TypeError", "Handler name must be a string"));
            };
            (None, name, operands.collect::<Vec<_>>())
        }
        Called::Custom { ns, name } => (Some(ns), name, operands),
    };
    let mut args = Vec::with_capacity(operands.len());
    for operand in operands {
        args.push(operand_value(req, this, operand)?);
    }
    let mode = match &mode_ns {
        None => Some(&state.php_ns),
        Some(ns) => state
            .namespaces
            .iter()
            .find(|(n, _)| n == ns)
            .map(|(_, m)| m),
    };
    let callable = match mode {
        None | Some(Mode::None) => Err(throw(req, "Error", "No callbacks were registered")),
        Some(Mode::All) => {
            let name_value = string(req, &name)?;
            let found = callable_checked(req, &name_value);
            req.release(name_value);
            found.map_err(|why| {
                let shown = String::from_utf8_lossy(&name).into_owned();
                throw(req, "Error", &format!("Invalid callback {shown}, {why}"))
            })
        }
        Some(Mode::Set(functions)) => {
            if let Some((_, c, _)) = functions.iter().find(|(n, _, _)| *n == name) {
                Ok(c.clone())
            } else {
                let shown = String::from_utf8_lossy(&name).into_owned();
                Err(throw(
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
                req.release(a);
            }
            return Err(thrown);
        }
    };
    let answered = call(req, &callable, &args);
    for a in args {
        if a.as_object().is_some() || a.as_array().is_some() {
            state.node_list.push(a);
        } else {
            req.release(a);
        }
    }
    let answer = answered?;
    let returned = match answer.as_ref() {
        ValueRef::Object(o) if crate::instance_of(req, *o, "DOMNode") => {
            match crate::node_of(req, *o) {
                Some(node) => {
                    state.node_list.push(answer);
                    return Ok(Returned::Node(node));
                }
                None => Returned::Sentinel,
            }
        }
        ValueRef::Bool(b) => Returned::Bool(b),
        ValueRef::Object(_) => {
            req.release(answer);
            return Err(throw(
                req,
                "TypeError",
                "Only objects that are instances of DOM nodes can be converted to an XPath expression",
            ));
        }
        _ => Returned::Str(to_string(req, &answer)?.as_bytes().to_vec()),
    };
    req.release(answer);
    Ok(returned)
}

/// `php_xpath_eval`: `query` forces a node set, `evaluate` answers what
/// the expression does.
fn eval(req: &mut Request, force_node_set: bool) -> Result<Value, Thrown> {
    arity(req, 1, 3)?;
    let expr = bytes_arg(req, 1)?;
    let context = object_arg(req, 2)?;
    let this = this(req)?;
    let default_ns = matches!(dom(req, this), Some(d) if matches!(&d.extra, Extra::XPath(s) if s.register_node_ns));
    let register = bool_arg(req, 3, default_ns)?;
    let Some(mut state) = take(req, this) else {
        return Err(throw(req, "Error", "Invalid XPath Context"));
    };
    let Some(doc) = state.ctx.doc() else {
        put(req, this, state);
        docref(req, Level::Warning, "Invalid XPath Document Pointer");
        return Ok(Value::bool(false));
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
        return Err(throw(req, "Error", "Node from wrong document"));
    }
    let (answer, raised, thrown) = {
        let XPathState { ctx, callbacks, .. } = &mut *state;
        let mut handler = |req: &mut Request, called: Called, operands: Vec<Operand>| {
            dispatch(req, this, callbacks, called, operands)
        };
        ctx.eval(req, &expr, node, register, &mut handler)
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
        return Ok(Value::bool(false));
    };
    let answer = if force_node_set && !matches!(answer, Answer::NodeSet(_)) {
        Answer::NodeSet(Vec::new())
    } else {
        answer
    };
    match answer {
        Answer::NodeSet(items) => {
            let mut array = PhpArray::new(req)?;
            for item in items {
                let v = item_object(req, this, item)?;
                array.append(req, v)?;
            }
            crate::node_set(req, array)
        }
        Answer::Bool(b) => Ok(Value::bool(b)),
        Answer::Number(n) => Ok(Value::float(n)),
        Answer::Str(s) => string(req, &s),
        Answer::Other => Ok(Value::null()),
    }
}

/// `DOMXPath::query(string $expression, ?DOMNode $contextNode = null, bool $registerNodeNS = true): mixed`.
fn query(req: &mut Request) -> Result<Value, Thrown> {
    eval(req, true)
}

/// `DOMXPath::evaluate(...)`.
fn evaluate(req: &mut Request) -> Result<Value, Thrown> {
    eval(req, false)
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
fn register_namespace(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let prefix = bytes_arg(req, 1)?;
    let uri = bytes_arg(req, 2)?;
    let this = this(req)?;
    let took = match dom(req, this) {
        Some(d) => match &d.extra {
            Extra::XPath(state) => Some(state.ctx.register_ns(&prefix, &uri)),
            _ => None,
        },
        None => None,
    };
    match took {
        Some(ok) => Ok(Value::bool(ok)),
        None => Err(throw(req, "Error", "Invalid XPath Context")),
    }
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
/// ```
/// ```expect
/// string(1) "X"
/// string(2) "X!"
/// No callback handler "strrev" registered
/// ```
fn register_php_functions(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 1)?;
    let restrict = crate::arg(req, 1)?;
    let this = this(req)?;
    let Some(mut state) = take(req, this) else {
        return Err(throw(req, "Error", "Invalid XPath Context"));
    };
    let answered = register_into(req, &mut state.callbacks.php_ns, restrict);
    put(req, this, state);
    answered?;
    Ok(Value::null())
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
    if let Some(array) = restrict.as_array().copied() {
        for (key, value) in &array {
            let name = match key {
                Key::Str(s) => s.as_bytes().to_vec(),
                Key::Int(_) => to_string(req, &value)?.as_bytes().to_vec(),
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
fn register_php_function_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 3, 3)?;
    let ns = crate::path_arg(req, 1)?;
    let name = crate::path_arg(req, 2)?;
    let given = crate::arg(req, 3)?.unwrap_or_default();
    let this = this(req)?;
    if ns == b"http://php.net/xpath" {
        return Err(arg_value_error(
            req,
            1,
            "not be \"http://php.net/xpath\" because it is reserved by PHP",
        ));
    }
    if name.is_empty() || !Name::valid_ncname(&name) {
        return Err(arg_value_error(req, 2, "be a valid callback name"));
    }
    let (callable, value) = callable_of(req, &given, 3, false)?;
    let Some(mut state) = take(req, this) else {
        req.release(value);
        return Err(throw(req, "Error", "Invalid XPath Context"));
    };
    state.ctx.register_custom(&ns, &name);
    let at = if let Some(at) = state
        .callbacks
        .namespaces
        .iter()
        .position(|(n, _)| *n == ns)
    {
        at
    } else {
        state.callbacks.namespaces.push((ns, Mode::None));
        state.callbacks.namespaces.len() - 1
    };
    let mut functions = match std::mem::take(&mut state.callbacks.namespaces[at].1) {
        Mode::Set(f) => f,
        _ => Vec::new(),
    };
    if let Some(slot) = functions.iter_mut().find(|(n, _, _)| *n == name) {
        let old = std::mem::replace(&mut slot.2, value);
        slot.1 = callable;
        req.release(old);
    } else {
        functions.push((name, callable, value));
    }
    state.callbacks.namespaces[at].1 = Mode::Set(functions);
    put(req, this, state);
    Ok(Value::null())
}

/// `DOMXPath::quote(string $str): string`.
///
/// ```php
/// echo DOMXPath::quote("a"), ' ', DOMXPath::quote("it's"), ' ', DOMXPath::quote("'\"\""), "\n";
/// ```
/// ```expect
/// 'a' "it's" concat("'",'""')
/// ```
fn quote(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let input = crate::path_arg(req, 1)?;
    if !input.contains(&b'\'') {
        return string(req, &[b"'".as_slice(), &input, b"'"].concat());
    }
    if !input.contains(&b'"') {
        return string(req, &[b"\"".as_slice(), &input, b"\""].concat());
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
    string(req, &out)
}

/// The methods, `Class::method` to entry.
pub(crate) const METHODS: &[(&str, Entry)] = &[
    ("DOMXPath::__construct", construct),
    ("DOMXPath::evaluate", evaluate),
    ("DOMXPath::query", query),
    ("DOMXPath::registerNamespace", register_namespace),
    ("DOMXPath::registerPhpFunctions", register_php_functions),
    ("DOMXPath::registerPhpFunctionNS", register_php_function_ns),
    ("DOMXPath::quote", quote),
];

/// `dom_xpath_object_handlers`: the dom table with no clone, and the
/// evaluator's registrations given back when the object goes.
pub(crate) struct XPathHandlers;

pub(crate) static HANDLERS: XPathHandlers = XPathHandlers;

impl php_ext::abi::ObjectHandlers for XPathHandlers {
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: php_ext::abi::PropertyRead,
    ) -> Result<Value, Thrown> {
        crate::read_property(req, object, name, read)
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: php_ext::values::PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        crate::write_property(req, object, name, value)
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> php_ext::abi::PropertyPlace {
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
    ) -> Result<Option<PhpArray>, Thrown> {
        crate::debug_info(req, object)
    }

    fn clone_obj(&self) -> Option<php_ext::abi::CloneObj> {
        None
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        if let Some(state) = take(req, object) {
            state.callbacks.release(req);
        }
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(d) = dom(req, object)
            && let Extra::XPath(state) = &d.extra
        {
            state.callbacks.visit(visit);
        }
    }
}
