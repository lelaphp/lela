//! `DOMXPath` (`xpath.c`, `xpath_callbacks.c`): a libxml2 `XPath`
//! evaluator over one document, its namespaces, and the php functions an
//! expression may call through `php:function`, `php:functionString` and
//! the names `registerPhpFunctionNS` registered.

use php_ext::abi::{Callable, GcTable, Instance, Passed, Path, Raw, Refusals, Std, This, Union};
use php_ext::attr::php_class;
use php_ext::call::call;
use php_ext::coerce::{callable_checked, with_string};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, docref, pending, throw_error,
    throw_error_chained,
};
use php_ext::request::{Foreign, Request};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Emalloc, Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};
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

/// `PHP_DOM_REG_FUNC_MODE_*`: how one namespace serves a call.
///
/// php lays a namespace down at `PHP_DOM_REG_FUNC_MODE_NONE`, which its
/// dispatch serves as `SET` (the `ZEND_ASSERT` that tells them apart is
/// gone from a release build): a refused registration leaves the
/// namespace there, and a call looks the table up all the same.
#[derive(Default)]
enum Mode {
    /// A call is served from the table, by name.
    #[default]
    Set,
    /// `registerPhpFunctions()` with no argument: any function by name,
    /// the table kept for a later set.
    All,
}

/// One entry of a namespace's `functions` table.
struct Function {
    /// The name an expression calls it by.
    name: Vec<u8>,
    /// The callable, kept as php keeps the entry's
    /// `zend_fcall_info_cache` (`Z/ext/dom/xpath_callbacks.c:32-34`).
    callable: Owner<Callable>,
}

/// `php_dom_xpath_callback_ns`: one namespace's `functions` table,
/// filled as a registration walks its argument, and the `mode` beside it.
#[derive(Default)]
struct Namespace {
    /// `functions`, in the order names were first registered.
    functions: Vec<Function>,
    /// `mode`, which the table outlives.
    mode: Mode,
}

impl Namespace {
    /// `zend_hash_update(&ns->functions, name, ..)`: `name` registered,
    /// in the place it already held, the owner it replaces let go of.
    fn insert(&mut self, req: &mut Request, function: Function) {
        if let Some(slot) = self.functions.iter_mut().find(|f| f.name == function.name) {
            std::mem::replace(slot, function).callable.give_up(req);
        } else {
            self.functions.push(function);
        }
    }

    /// The callable registered as `name`.
    fn find(&self, name: &[u8]) -> Option<&Callable> {
        self.functions
            .iter()
            .find(|f| f.name == name)
            .map(|f| &*f.callable)
    }

    /// `php_dom_xpath_callback_ns_update_method_handler`: each entry of
    /// an array, or the one name, registered as it is read, so a refusal
    /// leaves the ones before it in the table; no argument sets the `mode`
    /// alone.
    fn update(&mut self, req: &mut Request, restrict: Option<Value>) -> Result<(), Thrown> {
        let Some(restrict) = restrict else {
            self.mode = Mode::All;
            return Ok(());
        };
        if let Some(array) = restrict.as_array().copied() {
            // `ZEND_HASH_FOREACH_STR_KEY_VAL`, holding nothing: a
            // conversion may serve `__toString`.
            walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
                let value = *row.value();
                let callable = callable_of(req, &value, 1, true)?;
                let name = match row.key() {
                    Key::Str(s) => Ok(s.as_bytes().to_vec()),
                    Key::Int(_) => with_string(req, &value, <[u8]>::to_vec),
                };
                let name = match name {
                    Ok(name) if Validation::Nulls.holds(&name) => name,
                    Ok(_) => {
                        callable.give_up(req);
                        let tail = "be an array containing valid callback names";
                        return Err(arg_value_error(req, 1, tail));
                    }
                    Err(thrown) => {
                        callable.give_up(req);
                        return Err(thrown);
                    }
                };
                self.insert(req, Function { name, callable });
                Ok(std::ops::ControlFlow::Continue(()))
            })?;
        } else {
            let name = restrict
                .as_str_bytes()
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            if !Validation::Nulls.holds(&name) {
                return Err(arg_value_error(req, 1, "be a valid callback name"));
            }
            let callable = callable_of(req, &restrict, 1, false)?;
            self.insert(req, Function { name, callable });
        }
        self.mode = Mode::Set;
        Ok(())
    }
}

/// `php_dom_xpath_callback_name_validation`: what a callback's name is
/// held to.
#[derive(Clone, Copy)]
enum Validation {
    /// `PHP_DOM_XPATH_CALLBACK_NAME_VALIDATE_NULLS`: no NUL byte.
    Nulls,
    /// `PHP_DOM_XPATH_CALLBACK_NAME_VALIDATE_NCNAME`: an `NCName`.
    NcName,
}

impl Validation {
    /// `php_dom_xpath_is_callback_name_valid`: `name` is not empty and
    /// passes this validation.
    fn holds(self, name: &[u8]) -> bool {
        !name.is_empty()
            && !name.contains(&0)
            && (matches!(self, Validation::Nulls) || Name::valid_ncname(name))
    }
}

/// `dom_xpath_object`'s own fields. The evaluator stays here for the
/// span of an evaluation: the request keeps libxml's evaluator
/// ([`Request::adopt`], ADR-0013) and the evaluation holds it, so PHP
/// code a function serves reaches the object as php's does, evaluating
/// again, registering on it or constructing it anew (#2318).
pub(crate) struct XPathState {
    ctx: Foreign<ffi::XPath>,
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
    /// `php_ns`, the functions of `php:function`, laid down by the first
    /// `registerPhpFunctions`; boxed as php allocates it, so the owner's
    /// payload fits the struct php allocates (`#[php_class(size = ..)]`,
    /// `XSLTProcessor`'s among them).
    php_ns: Option<Box<Namespace>>,
    /// `namespaces`: the functions `registerPhpFunctionNS` registered, by
    /// namespace in registration order.
    namespaces: Vec<(Vec<u8>, Namespace)>,
    /// `node_list`: the values a call handed a function or had back, kept
    /// alive as long as the evaluator, its owner of each.
    node_list: Vec<Owner<Value>>,
}

impl XPathCallbacks {
    /// `php_dom_xpath_callbacks_dtor`: every registered function and kept
    /// value let go of.
    pub fn release(self, req: &mut Request) {
        let namespaces = self
            .php_ns
            .map(|n| *n)
            .into_iter()
            .chain(self.namespaces.into_iter().map(|(_, n)| n));
        for namespace in namespaces {
            for function in namespace.functions {
                function.callable.give_up(req);
            }
        }
        for value in self.node_list {
            value.give_up(req);
        }
    }

    /// `php_dom_xpath_callbacks_get_gc`: every value the table holds.
    pub fn visit(&self, visit: &mut dyn FnMut(Value)) {
        let namespaces = self.php_ns.as_deref().into_iter();
        for namespace in namespaces.chain(self.namespaces.iter().map(|(_, n)| n)) {
            for function in &namespace.functions {
                function.callable.visit(visit);
            }
        }
        for value in &self.node_list {
            visit(**value);
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
        let restrict = restrict
            .map(|v| v.deref())
            .filter(|v| !matches!(v.as_ref(), ValueRef::Null));
        self.php_ns.get_or_insert_default().update(req, restrict)
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
        if !Validation::NcName.holds(name) {
            return Err(arg_value_error(req, 2, "be a valid callback name"));
        }
        let callable = callable_of(req, callable, 3, false)?;
        let at = if let Some(at) = self.namespaces.iter().position(|(n, _)| n == ns) {
            at
        } else {
            self.namespaces.push((ns.to_vec(), Namespace::default()));
            self.namespaces.len() - 1
        };
        let namespace = &mut self.namespaces[at].1;
        let name = name.to_vec();
        namespace.insert(req, Function { name, callable });
        namespace.mode = Mode::Set;
        Ok(())
    }

    /// `php_dom_xpath_callbacks_delayed_lib_registration`'s walk: each
    /// namespace and name `registerPhpFunctionNS` registered, which the
    /// owner registers on an evaluator of its own.
    #[must_use]
    pub fn registered(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        for (ns, namespace) in &self.namespaces {
            out.extend(
                namespace
                    .functions
                    .iter()
                    .map(|f| (ns.clone(), f.name.clone())),
            );
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
        req: &mut Request,
        at: CallbacksAt,
        called: Called,
        operands: Vec<Operand>,
        modern: bool,
        proxy: &mut Proxy<'_>,
    ) -> Result<Returned, Thrown> {
        dispatch(req, at, called, operands, modern, proxy)
    }
}

/// Where an owner keeps its [`XPathCallbacks`]: the object, and how its
/// payload is reached, read afresh at each step of a call, since the PHP
/// code a call serves may register on them, replace them or free them,
/// as php's table is the object's own throughout (#2318, #2319).
#[derive(Clone, Copy)]
pub struct CallbacksAt {
    pub object: PhpObject,
    pub reach: fn(&mut Request, PhpObject) -> Option<&mut XPathCallbacks>,
}

impl CallbacksAt {
    fn get(self, req: &mut Request) -> Option<&mut XPathCallbacks> {
        (self.reach)(req, self.object)
    }

    /// `php_dom_xpath_callbacks_clean_node_list`: the values the calls of
    /// one evaluation handed out let go of, taken off the table first,
    /// since letting one go can call a destructor that reaches it.
    pub fn clean_node_list(self, req: &mut Request) {
        let Some(callbacks) = self.get(req) else {
            return;
        };
        for value in std::mem::take(&mut callbacks.node_list) {
            value.give_up(req);
        }
    }

    /// `zend_hash_next_index_insert(&ns->node_list, ..)`: `value` kept
    /// alive with the callbacks, let go of where they are gone.
    fn keep(self, req: &mut Request, value: Owner<Value>) {
        match self.get(req) {
            Some(callbacks) => callbacks.node_list.push(value),
            None => value.give_up(req),
        }
    }
}

/// `php_dom_xpath_callbacks_proxy_factory`: the object a node handed to a
/// php function stands as.
pub type Proxy<'a> = dyn FnMut(&mut Request, Node) -> Result<Owner<Value>, Thrown> + 'a;

/// The evaluator of `this`, where it has been constructed.
fn state(req: &mut Request, this: PhpObject) -> Option<&mut XPathState> {
    match &mut dom_mut(req, this)?.extra {
        Extra::XPath(state) => Some(state),
        _ => None,
    }
}

/// The callbacks of `this`, as [`CallbacksAt`] reaches them.
fn callbacks(req: &mut Request, this: PhpObject) -> Option<&mut XPathCallbacks> {
    state(req, this).map(|state| &mut state.callbacks)
}

/// The evaluator of `this` taken off it, as it is freed or constructed
/// anew: libxml's evaluator given up, closed now or as the evaluation that
/// holds it ends, and its registrations let go of.
fn discard(req: &mut Request, this: PhpObject) {
    let Some(d) = dom_mut(req, this) else {
        return;
    };
    let Extra::XPath(state) = std::mem::take(&mut d.extra) else {
        return;
    };
    let XPathState { ctx, callbacks, .. } = *state;
    drop(req.disown(ctx));
    callbacks.release(req);
}

/// `f` over the callbacks of `this`, taken off it for a registration
/// that can call PHP code (a name's `__toString`, an autoloader) and put
/// back after, in place of any a constructor laid meanwhile.
fn registering<R>(
    req: &mut Request,
    this: PhpObject,
    f: impl FnOnce(&mut Request, &mut XPathCallbacks) -> R,
) -> Option<R> {
    let mut taken = std::mem::take(&mut state(req, this)?.callbacks);
    let answer = f(req, &mut taken);
    match state(req, this) {
        Some(state) => std::mem::replace(&mut state.callbacks, taken).release(req),
        None => taken.release(req),
    }
    Some(answer)
}

/// `dom_xpath_document_read`.
fn document_read(req: &mut Request, object: PhpObject) -> Read {
    let id = state(req, object).map(|state| state.ctx.id());
    let doc = id.and_then(|id| req.foreign(id)).and_then(|ctx| ctx.doc());
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
    Ok(Some(Owner::bool(on)))
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
                return Ok(Owner::null());
            };
            let parent_value = proxy(req, parent)?;
            let Some(parent_obj) = parent_value.as_object().copied() else {
                parent_value.give_up(req);
                return Ok(Owner::null());
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
                // The parent's owner is the namespace node's from here.
                d.extra = Extra::NamespaceParent(parent_value);
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
        Operand::Bool(b) => Ok(Owner::bool(b)),
        Operand::Number(n) => Ok(Owner::float(n)),
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
                array.append(req, v)?;
            }
            Ok(array.into_value())
        }
    }
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

/// What a namespace's table answers for one name.
enum Lookup {
    /// No table for the namespace: no registration made one.
    NoTable,
    /// `PHP_DOM_REG_FUNC_MODE_ALL`: any function by name.
    Any,
    /// The callable registered as the name, if one is.
    Named(Option<Callable>),
}

impl Lookup {
    /// What `callbacks` answers for `name` in `ns`, php's namespace where
    /// none is given.
    fn of(callbacks: Option<&XPathCallbacks>, ns: Option<&[u8]>, name: &[u8]) -> Lookup {
        let namespace = match (ns, callbacks) {
            (_, None) => None,
            (None, Some(callbacks)) => callbacks.php_ns.as_deref(),
            (Some(ns), Some(callbacks)) => callbacks
                .namespaces
                .iter()
                .find(|(n, _)| n == ns)
                .map(|(_, namespace)| namespace),
        };
        match namespace {
            None => Lookup::NoTable,
            Some(Namespace {
                mode: Mode::All, ..
            }) => Lookup::Any,
            Some(namespace) => Lookup::Named(namespace.find(name).cloned()),
        }
    }
}

/// `php_dom_xpath_callback_dispatch`. The table is read where the call
/// is made and again where it keeps what the call handed out, since the
/// function called can register, replace or free it in between. A name
/// no callable answers to is `zend_call_function`'s refusal, chained
/// over what an autoloader threw:
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, "\n"; }
/// spl_autoload_register(function ($c) { throw new RuntimeException('boom'); });
/// $d = new DOMDocument; $d->loadXML('<a/>'); $xp = new DOMXPath($d);
/// $xp->registerNamespace('php', 'http://php.net/xpath'); $xp->registerPhpFunctions();
/// pk(function () use ($xp) { try { $xp->evaluate("php:function('Foo::bar')"); } catch (Error $e) {} });
/// try { $xp->evaluate("php:function('Foo::bar')"); } catch (Error $e) { echo $e->getMessage(), " / ", $e->getPrevious()->getMessage(), "\n"; }
/// ```
/// ```expect
/// 5552
/// Invalid callback Foo::bar, class "Foo" not found / boom
/// ```
fn dispatch(
    req: &mut Request,
    at: CallbacksAt,
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
    let callbacks = at.get(req).map(|callbacks| &*callbacks);
    let callable = match Lookup::of(callbacks, mode_ns.as_deref(), &name) {
        Lookup::NoTable => Err(throw_error(req, "Error", "No callbacks were registered")),
        Lookup::Any => {
            // `fci.function_name`, released once `zend_call_function`
            // returns, after its refusal is thrown.
            let name_value = string(req, &name)?;
            let found = callable_checked(req, &name_value).map_err(|why| {
                let shown = String::from_utf8_lossy(&name).into_owned();
                let message = format!("Invalid callback {shown}, {why}");
                // `zend_is_callable_ex`'s `error`, `zend_spprintf`'s
                // buffer, which `zend_call_function` frees after its throw.
                let error = match Emalloc::printf_buffer(req, why.len()) {
                    Ok(error) => error,
                    Err(refused) => return refused,
                };
                // An autoloader's throw is the refusal's previous, as
                // `zend_call_function`'s `zend_throw_error` chains onto
                // `EG(exception)`, its `zend_vspprintf` buffer standing
                // across the throw.
                let thrown = match pending(req) {
                    Err(previous) => throw_error_chained(req, previous, "Error", &message),
                    Ok(()) => throw_error(req, "Error", &message),
                };
                error.free(req);
                thrown
            });
            name_value.give_up(req);
            found
        }
        Lookup::Named(Some(callable)) => Ok(callable),
        Lookup::Named(None) => {
            let shown = String::from_utf8_lossy(&name).into_owned();
            Err(throw_error(
                req,
                "Error",
                &format!("No callback handler \"{shown}\" registered"),
            ))
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
            at.keep(req, a);
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
                    at.keep(req, answer);
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
        _ => with_string(req, &answer, <[u8]>::to_vec).map(Returned::Str),
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
    let Some(id) = state(req, this).map(|state| state.ctx.id()) else {
        return Err(throw_error(req, "Error", "Invalid XPath Context"));
    };
    let Some(doc) = req.foreign(id).and_then(|ctx| ctx.doc()) else {
        if modern {
            return Err(throw_error(req, "Error", "Invalid XPath Document Pointer"));
        }
        docref(req, Level::Warning, "Invalid XPath Document Pointer");
        return Ok(Owner::bool(false));
    };
    let node = match context {
        Some(o) => Some(fetch(req, o)?),
        None => doc.root(),
    };
    if node.is_some_and(|n| n.doc() != Some(doc)) {
        return Err(throw_error(req, "Error", "Node from wrong document"));
    }
    let in_scope = match node {
        Some(n) if register && modern => {
            crate::private::with_mapper(req, this, |mapper, req| mapper.in_scope(req, n, false))?
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };
    let scope = match (register, modern) {
        (false, _) => Scope::None,
        (true, false) => Scope::InScope,
        (true, true) => Scope::List(&in_scope),
    };
    let at = CallbacksAt {
        object: this,
        reach: callbacks,
    };
    let mut proxy = |req: &mut Request, node: Node| object_for(req, node, Some(this));
    let mut handler = |req: &mut Request, called: Called, operands: Vec<Operand>| {
        XPathCallbacks::call(req, at, called, operands, modern, &mut proxy)
    };
    let Some((answer, raised, thrown)) =
        ffi::XPath::eval_held(req, id, expr, node, scope, &mut handler)
    else {
        return Err(throw_error(req, "Error", "Invalid XPath Context"));
    };
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
        return Ok(Owner::bool(false));
    };
    let answer = if force_node_set && !matches!(answer, Answer::NodeSet(_)) {
        Answer::NodeSet(Vec::new())
    } else {
        answer
    };
    match answer {
        Answer::NodeSet(items) => node_list(req, this, items, modern),
        Answer::Bool(b) => Ok(Owner::bool(b)),
        Answer::Number(n) => Ok(Owner::float(n)),
        Answer::Str(s) => string(req, &s),
        Answer::Other => Ok(Owner::null()),
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
        array.append(req, v)?;
    }
    let class = if modern {
        "Dom\\NodeList"
    } else {
        "DOMNodeList"
    };
    crate::node_set(req, class, array)
}

/// One callable the registration read, kept: `zend_fcc_dup`'s owners
/// of its value and of the receiver it resolved to.
fn callable_of(
    req: &mut Request,
    value: &Value,
    position: u32,
    is_array: bool,
) -> Result<Owner<Callable>, Thrown> {
    match callable_checked(req, value) {
        Ok(c) => Ok(Owner::take(req, c)),
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
        discard(req, this);
        let shared = crate::shared_doc(req, doc_obj);
        if let Some(mut obj) = crate::libxml_half(req, this) {
            obj.decrement_doc_ref(req);
            obj.share(req, shared, doc);
        }
        let state = XPathState {
            ctx: req.adopt(ctx),
            register_node_ns: registerNodeNS,
            callbacks: XPathCallbacks::default(),
        };
        match dom_mut(req, this) {
            Some(d) => d.extra = Extra::XPath(Box::new(state)),
            None => drop(req.disown(state.ctx)),
        }
        Ok(())
    }

    /// `DOMXPath::evaluate(string $expression, ?DOMNode $contextNode = null, bool $registerNodeNS = true): mixed`;
    /// an omitted `$registerNodeNS` is the evaluator's `registerNodeNamespaces`.
    ///
    /// The evaluator stays on the object while it evaluates, so a
    /// function the expression calls evaluates on it again, and what it
    /// registers holds for the rest of the evaluation and after (#2318).
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r><a>1</a><b>2</b></r>');
    /// $x = new DOMXPath($d);
    /// function inner($n) {
    ///     global $x;
    ///     var_dump($x->evaluate('count(//b)'), $x->registerNamespace('p', 'urn:p'));
    ///     $x->registerPhpFunctions(['strtoupper']);
    ///     return 'v';
    /// }
    /// $x->registerNamespace('php', 'http://php.net/xpath');
    /// $x->registerPhpFunctions(['inner']);
    /// var_dump($x->evaluate('string(php:function("inner", //a))'));
    /// var_dump($x->evaluate('count(//p:z)'), $x->evaluate('php:function("strtoupper", "x")'));
    /// ```
    /// ```expect
    /// float(1)
    /// bool(true)
    /// string(1) "v"
    /// float(0)
    /// string(1) "X"
    /// ```
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
        let id = state(req, this.object()).map(|state| state.ctx.id());
        let took = id
            .and_then(|id| req.foreign(id))
            .map(|ctx| ctx.register_ns(prefix.as_bytes(), namespace.as_bytes()));
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
    ///
    /// The namespace's table is filled as the array is walked, so a
    /// refused entry leaves the ones before it registered, and a refusal
    /// lays the namespace down; a call with no argument lets any function
    /// be called and keeps the table, which a later set calls by name
    /// again.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r><a>x</a></r>');
    /// $x = new DOMXPath($d);
    /// $x->registerNamespace('php', 'http://php.net/xpath');
    /// try { $x->registerPhpFunctions(['f' => fn($s) => "f$s", 'g' => 'no_such_function']); } catch (TypeError $e) { echo get_class($e), "\n"; }
    /// var_dump($x->evaluate('php:functionString("f", //a)'));
    /// try { $x->evaluate('php:functionString("g", //a)'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $y = new DOMXPath($d);
    /// $y->registerNamespace('php', 'http://php.net/xpath');
    /// try { $y->registerPhpFunctions('no_such_function'); } catch (TypeError $e) { echo get_class($e), "\n"; }
    /// try { $y->evaluate('php:functionString("g", //a)'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $x->registerPhpFunctions(['up' => fn($s) => "U$s"]);
    /// $x->registerPhpFunctions();
    /// var_dump($x->evaluate('php:functionString("strtoupper", //a)'));
    /// try { $x->evaluate('php:functionString("up", //a)'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $x->registerPhpFunctions(['rev' => 'strrev']);
    /// var_dump($x->evaluate('php:functionString("up", //a)'), $x->evaluate('php:functionString("f", //a)'));
    /// try { $x->registerPhpFunctions(['' => 'no_such_function']); } catch (TypeError $e) { echo get_class($e), "\n"; }
    /// try { $x->registerPhpFunctions(['' => 'strlen']); } catch (ValueError $e) { echo get_class($e), "\n"; }
    /// ```
    /// ```expect
    /// TypeError
    /// string(2) "fx"
    /// No callback handler "g" registered
    /// TypeError
    /// No callback handler "g" registered
    /// string(1) "X"
    /// Invalid callback up, function "up" not found or invalid function name
    /// string(2) "Ux"
    /// string(2) "fx"
    /// TypeError
    /// ValueError
    /// ```
    #[php(tentative)]
    fn registerPhpFunctions(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] restrict: Option<Union<PhpString, PhpArray>>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let restrict = restrict.map(|given| match given {
            Union::A(name) => Value::string(name),
            Union::B(array) => Value::array(array),
        });
        let registered = registering(req, this, |req, callbacks| {
            callbacks.register_php(req, restrict)
        });
        match registered {
            Some(answered) => answered,
            None => Err(throw_error(req, "Error", "Invalid XPath Context")),
        }
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
    ///
    /// A function registered here or through `registerPhpFunctions` is
    /// kept as php keeps its `zend_fcall_info_cache`: a `'Class::method'`
    /// string registered with `$this` in scope keeps the object it
    /// resolved to until the evaluator lets it go.
    ///
    /// ```php
    /// class A {
    ///     function f() { return 'x'; }
    ///     function go() {
    ///         $x = new DOMXPath(new DOMDocument);
    ///         $x->registerPhpFunctionNS('urn:a', 'f', 'A::f');
    ///         $x->registerPhpFunctions(['g' => 'A::f']);
    ///         return $x;
    ///     }
    ///     function __destruct() { echo "dtor\n"; }
    /// }
    /// $x = (new A)->go();
    /// echo "kept\n";
    /// unset($x);
    /// echo "end\n";
    /// ```
    /// ```expect
    /// kept
    /// dtor
    /// end
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
        let value = callable.value();
        let Some(registered) = registering(req, this, |req, callbacks| {
            callbacks.register_ns(req, &ns, &name, &value)
        }) else {
            return Err(throw_error(req, "Error", "Invalid XPath Context"));
        };
        if registered.is_ok() {
            let id = state(req, this).map(|state| state.ctx.id());
            if let Some(ctx) = id.and_then(|id| req.foreign(id)) {
                ctx.register_custom(&ns, &name);
            }
        }
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

    /// `dom_xpath_objects_free_storage`: `zend_object_std_dtor`, then
    /// `xmlXPathFreeContext`, the document let go of and the callbacks
    /// given up.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        Std.free_obj(req, object);
        discard(req, object);
        crate::release_libxml(req, object);
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(d) = dom(req, object)
            && let Extra::XPath(state) = &d.extra
        {
            state.callbacks.visit(visit);
        }
        GcTable::Properties
    }
}
