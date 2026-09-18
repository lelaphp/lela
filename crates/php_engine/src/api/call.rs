//! The calling-back group (design §4.4): re-entry from a native into PHP
//! code, always through `&mut Request` (principle 9, ADR-0002). A
//! callable is served here, a native through its entry and a user
//! function by entering the engine's one loop again on the callee's
//! frame; the items that only read the linked tables answer from what
//! the request holds, and the rest still raise php's `Error` for an
//! undefined function or class.

use php_unit::decl::ConstExpr;

use crate::api::abi::Callable;
use crate::api::raising::{Thrown, fatal, throw};
use crate::api::request::Request;
use crate::api::values::{ClassId, PhpObject, PhpString, Value, ValueRef};

/// A linked function: user or native, by identity. A native method is
/// one too, numbered past the functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Function(pub(crate) u32);

impl Function {
    /// `zend_pass_function`: the callee `NEW` serves for a class with
    /// no constructor and arguments to take, which takes them and
    /// returns `null`.
    pub(crate) const PASS: Function = Function(u32::MAX);
}

/// A position in an `iterable`: an array's elements, or a `Traversable`
/// served through its iterator methods.
#[derive(Debug, Clone)]
pub struct Iter {
    entries: Vec<(Value, Value)>,
    at: Option<usize>,
}

impl Iter {
    /// Advance one element, serving `Iterator::next()` for an
    /// object; `false` past the end.
    pub fn next(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        let _ = req;
        let at = self.at.map_or(0, |i| i + 1);
        self.at = Some(at);
        Ok(at < self.entries.len())
    }

    /// The key at the position.
    #[must_use]
    pub fn key(&self) -> Option<&Value> {
        self.entries.get(self.at?).map(|(k, _)| k)
    }

    /// The value at the position.
    #[must_use]
    pub fn value(&self) -> Option<&Value> {
        self.entries.get(self.at?).map(|(_, v)| v)
    }
}

fn no_loop(req: &mut Request, what: &str) -> Thrown {
    throw(
        req,
        "Error",
        &format!("Call to undefined function {what}()"),
    )
}

/// The callee a `Callable` names, `zend_is_callable_ex` at the top of
/// `zend_call_function` (`Z/Zend/zend_execute_API.c:824`): the function
/// table's entry under the name the `Callable` carries, which is how
/// `zend_fcall_info_init` spelled it. A name the table has not is php's
/// `Error` for an invalid callback, worded as `zend_is_callable_ex`
/// words a string's miss; the callables the request hands out are
/// resolved when they are made, so only one made before its function
/// was declared reaches it.
fn function_of(req: &mut Request, callable: &Callable) -> Result<Function, Thrown> {
    let name = callable.name();
    match req.function_named(name.as_bytes()) {
        Some(function) => Ok(function),
        None => Err(throw(
            req,
            "Error",
            &format!(
                "Invalid callback {name}, function \"{name}\" not found or invalid function name"
            ),
        )),
    }
}

/// `zend_call_function` (`Z/Zend/zend_execute_API.c:806`): the callee
/// the callable names served on a call frame of its own, as
/// `zend_vm_stack_push_call_frame` and the loop over `fci->params`
/// build one. Each argument is one more owner the frame holds and is
/// released with it, so the caller keeps what it passed; the return is
/// the caller's, one owner; and what the callee left pending is the
/// `Err`, the frame popped before it is handed back, so a native reads
/// a callback's raise the way it reads its own.
///
/// A native is served through its entry, php's
/// `func->internal_function.handler` branch. A user function is served
/// by entering the engine's loop once more on the callee's frame,
/// php's `zend_execute_ex` branch (design §5.6, "Native→PHP re-entry
/// ... recurses via `execute(frame)` on the Rust stack"), which is the
/// same dispatch loop the call came from and the same tier.
///
/// The buffering layer is what calls a handler here
/// (`Z/main/output.c:944`), with the buffer and the
/// `PHP_OUTPUT_HANDLER_*` bits: two arguments,
/// so a native of one arity serves and one of another raises where php
/// raises.
///
/// ```php
/// ob_start('trim'); echo " abc ";
/// ```
/// ```expect
/// abc
/// ```
///
/// A user function serving as the handler: the buffer crosses into the
/// body, the body's return crosses back, and what it returned is what
/// comes out.
///
/// ```php
/// function shout($buffer, $mode) { return strtoupper($buffer); }
/// ob_start('shout');
/// echo "abc";
/// ```
/// ```expect
/// ABC
/// ```
///
/// A throwable the body leaves pending crosses the boundary the other
/// way: the handler is disabled and its buffer comes out whole
/// (`php_output_handler_op`'s `Failure`), the `ob_end_flush()` that
/// called it hands the throwable on, and the trace names the boundary
/// `[internal function]`, as `zend_trace_to_string` does for a frame no
/// opline called.
///
/// ```php
/// function boom($buffer, $mode) { return strtoupper(); }
/// ob_start('boom');
/// echo "abc";
/// ob_end_flush();
/// echo "after\n";
/// ```
/// ```expect exit=255
/// abc
/// Fatal error: Uncaught ArgumentCountError: strtoupper() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): strtoupper()
/// #1 [internal function]: boom('abc', 9)
/// #2 %s(5): ob_end_flush()
/// #3 {main}
///   thrown in %s on line 2
/// ```
pub fn call(req: &mut Request, callable: &Callable, arguments: &[Value]) -> Result<Value, Thrown> {
    let function = function_of(req, callable)?;
    req.serve_function(function, arguments)
}

/// `zend_call_function` with named arguments after the positional ones,
/// as its walk of `fci->named_params` places them
/// (`Z/Zend/zend_execute_API.c:894`): each one at the offset the callee
/// declares that name at, the slots between left `UNDEF` for the
/// prologue to fill, which is `zend_handle_named_arg`
/// (`Z/Zend/zend_execute.c:5518`). A name the callee declares no
/// parameter for is php's `Error` for an unknown named parameter, and a
/// name a positional argument already filled is php's other one.
pub fn call_named(
    req: &mut Request,
    callable: &Callable,
    arguments: &[Value],
    named: &[(PhpString, Value)],
) -> Result<Value, Thrown> {
    let function = function_of(req, callable)?;
    if named.is_empty() {
        return req.serve_function(function, arguments);
    }
    let placed = place_named(req, function, arguments, named)?;
    req.serve_function(function, &placed)
}

/// `zend_get_arg_offset_by_name` and `zend_handle_named_arg` over one
/// call's named arguments (`Z/Zend/zend_execute.c:5450`): the
/// positional arguments as passed, then each named one at the offset of
/// the parameter of that name, the slots it skipped `UNDEF`. php
/// collects a name a variadic callee does not declare into
/// `extra_named_params`, which no call frame here carries, so that one
/// is an `InternalError` and not a wrong placement.
fn place_named(
    req: &mut Request,
    function: Function,
    arguments: &[Value],
    named: &[(PhpString, Value)],
) -> Result<Vec<Value>, Thrown> {
    let parameters: Vec<(Vec<u8>, bool)> = req
        .function_decl(function)
        .map(|decl| {
            decl.parameters
                .iter()
                .map(|parameter| (parameter.name.0.to_vec(), parameter.variadic))
                .collect()
        })
        .unwrap_or_default();
    let mut placed = arguments.to_vec();
    for (name, value) in named {
        let wanted = name.as_bytes();
        let spelled = String::from_utf8_lossy(wanted).into_owned();
        let Some(at) = parameters
            .iter()
            .position(|(declared, variadic)| !variadic && declared == wanted)
        else {
            if parameters.iter().any(|(_, variadic)| *variadic) {
                return Err(fatal(
                    req,
                    &format!(
                        "InternalError: the named argument ${spelled} a variadic parameter collects"
                    ),
                ));
            }
            return Err(throw(
                req,
                "Error",
                &format!("Unknown named parameter ${spelled}"),
            ));
        };
        if placed
            .get(at)
            .is_some_and(|held| !matches!(held.as_ref(), ValueRef::Undef))
        {
            return Err(throw(
                req,
                "Error",
                &format!("Named parameter ${spelled} overwrites previous argument"),
            ));
        }
        if placed.len() <= at {
            placed.resize(at + 1, Value::undef());
        }
        placed[at] = *value;
    }
    Ok(placed)
}

/// `zend_call_method` on `object`: the method looked up on the
/// object's class by its lowered name, and served on a call frame with
/// the object as its receiver and the arguments as one more owner each,
/// the frame popped whether the entry returned or threw. A name the
/// class has no entry for is php's `Error` for an undefined method.
pub fn call_method(
    req: &mut Request,
    object: &PhpObject,
    method: &[u8],
    arguments: &[Value],
) -> Result<Value, Thrown> {
    let class = object.class();
    let Some(function) = req.method_named(class, &method.to_ascii_lowercase()) else {
        let class = req.class_name(class);
        return Err(throw(
            req,
            "Error",
            &format!(
                "Call to undefined method {class}::{}()",
                String::from_utf8_lossy(method)
            ),
        ));
    };
    req.serve_method(function, Some(*object), class, arguments)
}

/// `new $class(...$arguments)`: [`new_object`] and then the class's
/// constructor served on it with `arguments`, as `NEW` and its
/// `DO_FCALL` serve them; a class with none takes the arguments and
/// serves nothing (`zend_pass_function`).
pub fn instantiate(
    req: &mut Request,
    class: ClassId,
    arguments: &[Value],
) -> Result<PhpObject, Thrown> {
    let object = new_object(req, class)?;
    if let Some(constructor) = req.method_named(class, b"__construct") {
        let served = req.serve_method(constructor, Some(object), class, arguments);
        if let Err(thrown) = served {
            req.release(object.as_value());
            return Err(thrown);
        }
        req.release(served.unwrap_or_default());
    }
    Ok(object)
}

/// `object_init_ex`: an instance with its declared properties' defaults
/// and no constructor call, a store entry of the request's heap with
/// one owner, the caller's; an interface, a trait, an enum or an
/// abstract class is php's `Error` for what cannot be instantiated.
pub fn new_object(req: &mut Request, class: ClassId) -> Result<PhpObject, Thrown> {
    req.instantiable(class)?;
    req.new_object(class)
}

/// `clone`: the class's `clone_obj` (design §5.4), a fresh entry
/// holding what the object holds with one more owner each and `__clone`
/// served on it; a class with a null slot is php's `Error` for an
/// uncloneable object, and a `__clone` the scope may not reach is
/// `zend_bad_method_call`'s. The copy is the caller's, one owner.
pub fn clone(req: &mut Request, object: &PhpObject) -> Result<PhpObject, Thrown> {
    req.clone_object(*object, None)
}

/// `zend_lookup_class`: the linked class named, autoloading if needed.
pub fn lookup_class(req: &mut Request, name: &[u8]) -> Result<Option<ClassId>, Thrown> {
    Ok(req.class_named(name))
}

/// The linked function named, without autoloading.
#[must_use]
pub fn lookup_function(req: &Request, name: &[u8]) -> Option<Function> {
    req.function_named(name)
}

/// Serve the autoloaders for `name`, `zend_lookup_class` after the table miss.
pub fn autoload(req: &mut Request, name: &[u8]) -> Result<Option<ClassId>, Thrown> {
    let _ = name;
    let _ = req;
    Ok(None)
}

/// A constant by name, `zend_get_constant_ex`: `None` when undefined.
pub fn constant(req: &mut Request, name: &[u8]) -> Result<Option<Value>, Thrown> {
    Ok(req.constant_named(name))
}

/// A class constant, its expression evaluated on first use.
pub fn class_constant(req: &mut Request, class: ClassId, name: &[u8]) -> Result<Value, Thrown> {
    let _ = class;
    Err(throw(
        req,
        "Error",
        &format!("Undefined constant {}", String::from_utf8_lossy(name)),
    ))
}

/// Evaluate a constant expression in `scope`, `zval_update_constant_ex`.
pub fn eval_const_expr(
    req: &mut Request,
    expr: &ConstExpr,
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    let _ = scope;
    match expr {
        ConstExpr::Null => Ok(Value::null()),
        ConstExpr::Bool(b) => Ok(Value::bool(*b)),
        ConstExpr::Long(n) => Ok(Value::int(*n)),
        ConstExpr::Double(f) => Ok(Value::float(*f)),
        ConstExpr::Str(s) => Ok(Value::string(PhpString::new(req, s)?)),
        ConstExpr::Constant(name) => match req.constant_named(&name.0) {
            Some(value) => Ok(value),
            None => Err(throw(
                req,
                "Error",
                &format!(
                    "Undefined constant \"{}\"",
                    String::from_utf8_lossy(&name.0)
                ),
            )),
        },
        ConstExpr::Array(elements) => {
            let mut array = crate::api::values::PhpArray::new(req)?;
            for (key, value) in elements {
                let value = eval_const_expr(req, value, scope)?;
                match key {
                    Some(key) => {
                        let key = eval_const_expr(req, key, scope)?;
                        let key = crate::api::coerce::key_of(req, &key)?;
                        array.set(req, key, value)?;
                    }
                    None => array.append(req, value)?,
                }
            }
            Ok(Value::array(array))
        }
        _ => Err(throw(
            req,
            "Error",
            "Constant expression contains invalid operations",
        )),
    }
}

/// `foreach` over `value`: an array's elements now, a `Traversable`
/// through its iterator, a scalar php's `TypeError`.
pub fn iterate(req: &mut Request, value: &Value) -> Result<Iter, Thrown> {
    match value.as_array() {
        Some(array) => Ok(Iter {
            entries: array
                .snapshot()
                .into_iter()
                .map(|(k, v)| {
                    let key = match k {
                        crate::api::values::Key::Int(n) => Value::int(n),
                        crate::api::values::Key::Str(s) => Value::string(s),
                    };
                    (key, v)
                })
                .collect(),
            at: None,
        }),
        None => Err(throw(
            req,
            "TypeError",
            &format!(
                "foreach() argument must be of type array|object, {} given",
                value.type_name()
            ),
        )),
    }
}

/// `Closure::fromCallable`.
pub fn closure_from_callable(req: &mut Request, callable: &Callable) -> Result<PhpObject, Thrown> {
    Err(no_loop(req, &callable.name()))
}

/// Read a property through the class's `read_property` handler: a
/// view, `null` for one that is not there, as `zend_std_read_property`
/// answers with `BP_VAR_IS`.
pub fn read_prop(req: &mut Request, object: &PhpObject, name: &[u8]) -> Result<Value, Thrown> {
    Ok(object.prop(req, name).unwrap_or_default())
}

/// Write a property through the class's `write_property` (design
/// §5.4): the value is the object's from here.
pub fn write_prop(
    req: &mut Request,
    object: &PhpObject,
    name: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    let name = PhpString::new(req, name)?;
    object.set_prop(req, name, value)
}

/// Read a `static` property of `class`.
pub fn read_static_prop(req: &mut Request, class: ClassId, name: &[u8]) -> Result<Value, Thrown> {
    let _ = class;
    Err(throw(
        req,
        "Error",
        &format!(
            "Access to undeclared static property ::${}",
            String::from_utf8_lossy(name)
        ),
    ))
}

/// Write a `static` property of `class`. The value is the property's
/// from here; the skeleton has no property to keep it, so it is given
/// up once the read has raised what it raises.
pub fn write_static_prop(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    read_static_prop(req, class, name)?;
    req.release(value);
    Ok(())
}

/// `ReflectionClass::newLazyGhost`.
pub fn new_lazy_ghost(
    req: &mut Request,
    class: ClassId,
    initializer: &Callable,
) -> Result<PhpObject, Thrown> {
    let _ = initializer;
    new_object(req, class)
}

/// `ReflectionClass::newLazyProxy`.
pub fn new_lazy_proxy(
    req: &mut Request,
    class: ClassId,
    factory: &Callable,
) -> Result<PhpObject, Thrown> {
    let _ = factory;
    new_object(req, class)
}

/// `ReflectionClass::resetAsLazyGhost`.
pub fn reset_as_lazy_ghost(
    req: &mut Request,
    object: &PhpObject,
    initializer: &Callable,
) -> Result<(), Thrown> {
    let _ = (req, object, initializer);
    Ok(())
}

/// `ReflectionClass::resetAsLazyProxy`.
pub fn reset_as_lazy_proxy(
    req: &mut Request,
    object: &PhpObject,
    factory: &Callable,
) -> Result<(), Thrown> {
    let _ = (req, object, factory);
    Ok(())
}

/// `ReflectionClass::initializeLazyObject`: serve the initializer now.
pub fn initialize_lazy(req: &mut Request, object: &PhpObject) -> Result<(), Thrown> {
    let _ = (req, object);
    Ok(())
}

/// `ReflectionClass::markLazyObjectAsInitialized`.
pub fn skip_lazy_initialization(req: &mut Request, object: &PhpObject) -> Result<(), Thrown> {
    let _ = (req, object);
    Ok(())
}
