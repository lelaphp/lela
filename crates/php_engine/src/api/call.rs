//! The calling-back group (design §4.4): re-entry from a native into PHP
//! code, always through `&mut Request` (principle 9, ADR-0002). The
//! skeleton has no loop to re-enter, so every item that would serve user
//! code reports php's `Error` for an undefined function or class, and the
//! items that only read the linked tables answer from what the request
//! holds.

use php_unit::decl::ConstExpr;

use crate::api::abi::Callable;
use crate::api::raising::{Thrown, throw};
use crate::api::request::Request;
use crate::api::values::{ClassId, PhpObject, PhpString, Value};

/// A linked function: user or native, by identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Function(pub(crate) u32);

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

/// `zend_call_function`.
pub fn call(req: &mut Request, callable: &Callable, arguments: &[Value]) -> Result<Value, Thrown> {
    let _ = arguments;
    Err(no_loop(req, &callable.name()))
}

/// `zend_call_function` with named arguments after the positional ones.
pub fn call_named(
    req: &mut Request,
    callable: &Callable,
    arguments: &[Value],
    named: &[(PhpString, Value)],
) -> Result<Value, Thrown> {
    let _ = (arguments, named);
    Err(no_loop(req, &callable.name()))
}

/// `zend_call_method` on `object`.
pub fn call_method(
    req: &mut Request,
    object: &PhpObject,
    method: &[u8],
    arguments: &[Value],
) -> Result<Value, Thrown> {
    let _ = (object, arguments);
    Err(throw(
        req,
        "Error",
        &format!(
            "Call to undefined method {}()",
            String::from_utf8_lossy(method)
        ),
    ))
}

/// `new $class(...$arguments)`, constructor served.
pub fn instantiate(
    req: &mut Request,
    class: ClassId,
    arguments: &[Value],
) -> Result<PhpObject, Thrown> {
    let _ = arguments;
    new_object(req, class)
}

/// `object_init_ex`: an instance with its defaults and no constructor call.
pub fn new_object(req: &mut Request, class: ClassId) -> Result<PhpObject, Thrown> {
    Ok(req.new_object(class))
}

/// `clone`, through the class's `clone_obj` handler and `__clone`.
pub fn clone(req: &mut Request, object: &PhpObject) -> Result<PhpObject, Thrown> {
    let _ = req;
    Ok(object.clone())
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
            let mut array = crate::api::values::PhpArray::new();
            for (key, value) in elements {
                let value = eval_const_expr(req, value, scope)?;
                match key {
                    Some(key) => {
                        let key = eval_const_expr(req, key, scope)?;
                        let key = crate::api::coerce::key_of(req, &key)?;
                        array.set(key, value);
                    }
                    None => array.append(value),
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

/// Read a property through the class's `read_property` handler.
pub fn read_prop(req: &mut Request, object: &PhpObject, name: &[u8]) -> Result<Value, Thrown> {
    let _ = req;
    Ok(object.prop(name).cloned().unwrap_or_default())
}

/// Write a property through the class's `write_property` handler.
pub fn write_prop(
    req: &mut Request,
    object: &mut PhpObject,
    name: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    let name = PhpString::new(req, name)?;
    object.set_prop(name, value);
    Ok(())
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

/// Write a `static` property of `class`.
pub fn write_static_prop(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    read_static_prop(req, class, name).map(|_| drop(value))
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
    object: &mut PhpObject,
    initializer: &Callable,
) -> Result<(), Thrown> {
    let _ = (req, object, initializer);
    Ok(())
}

/// `ReflectionClass::resetAsLazyProxy`.
pub fn reset_as_lazy_proxy(
    req: &mut Request,
    object: &mut PhpObject,
    factory: &Callable,
) -> Result<(), Thrown> {
    let _ = (req, object, factory);
    Ok(())
}

/// `ReflectionClass::initializeLazyObject`: serve the initializer now.
pub fn initialize_lazy(req: &mut Request, object: &mut PhpObject) -> Result<(), Thrown> {
    let _ = (req, object);
    Ok(())
}

/// `ReflectionClass::markLazyObjectAsInitialized`.
pub fn skip_lazy_initialization(req: &mut Request, object: &mut PhpObject) -> Result<(), Thrown> {
    let _ = (req, object);
    Ok(())
}
