//! `ReflectionParameter` and the four type reflectors, the part of
//! `php_reflection.c` that reads one function's signature: php's
//! `parameter_reference` and `type_reference`, the two `intern->ptr`
//! payloads that are not a name a reflector can look up again.
//!
//! php hangs both off the reflection object; the engine has no such
//! pointer, so each is a row of the request state keyed by the
//! object's store index, written where php writes `intern->ptr` and
//! read where php's `GET_REFLECTION_OBJECT_PTR` reads it. An index is
//! reused only after its object is freed, and every reflector of
//! these classes is born through a factory here, so a row is either
//! this object's or gone.

use php_ext::abi::{Instance, This, Union, Untyped};
use php_ext::attr::php_class;
use php_ext::call::{Function, autoload, eval_const_expr, eval_declared, lookup_class};
use php_ext::coerce::{value_name, with_string};
use php_ext::decl::{ConstExpr, FunctionDecl, Parameter, Type};
use php_ext::raising::{Thrown, throw, throw_error, throw_exception_ex};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use super::{
    class_chain, class_name_of, class_named, lookup_function, new_object, parameter_string,
    reflection_class_factory, required_parameters, set_name, set_string_prop, this_name,
    type_to_string,
};

/// The function a `ReflectionParameter` reflects, `param->fptr` as a
/// name this engine can look the declaration up with again: a global
/// function, a method with the class that declared it, or a closure,
/// whose `{closure:…}` name no table holds, so what stands for it is
/// the object itself, as php's `intern->obj` does; or the `__invoke`
/// php makes for a closure on the lookup, over the closure, or over a
/// fresh one where the reflector was made from the class alone.
#[derive(Clone)]
pub(super) enum FunctionRef {
    Global(Vec<u8>),
    Method(ClassId, Vec<u8>),
    Closure(PhpObject),
    /// A closure's `__invoke`, over the closure's function
    /// ([`super::HeldMethod::Invoke`]).
    Invoke(Option<Function>),
}

impl FunctionRef {
    /// The closure a reflector over this function holds as php's
    /// `intern->obj`, which the declaration is read back off; none for
    /// a closure's `__invoke`, whose trampoline borrows the function.
    fn closure(&self) -> Option<PhpObject> {
        match *self {
            FunctionRef::Closure(closure) => Some(closure),
            FunctionRef::Global(_) | FunctionRef::Method(..) | FunctionRef::Invoke(_) => None,
        }
    }
}

/// php's `parameter_reference`: the function, the offset in its
/// parameter list, and whether the offset is below
/// `required_num_args`.
#[derive(Clone)]
pub(super) struct ParameterRef {
    function: FunctionRef,
    offset: usize,
    required: bool,
}

/// php's `type_reference`: the type and the `legacy_behavior` bit
/// `reflection_type_factory` sets for a top-level named type that is
/// neither `mixed` nor `null` alone, which is what makes
/// `ReflectionNamedType::getName()` drop the `?`.
#[derive(Clone)]
pub(super) struct TypeRef {
    ty: Type,
    legacy: bool,
}

/// `get_type_kind`: which of the three concrete type reflectors a
/// `zend_type` is rendered by.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Named,
    Union,
    Intersection,
}

/// One type as `zend_type` holds it: the members with `null` taken
/// out, whether `null` is in the mask, and whether the list is an
/// intersection. `Type::Nullable` and a `null` member of a union are
/// one bit in php, so they are one bit here.
struct Parts {
    members: Vec<Type>,
    nullable: bool,
    intersection: bool,
}

fn parts(ty: &Type) -> Parts {
    match ty {
        // `X&Y $p = null`: an intersection made nullable is the DNF
        // union `(X&Y)|null`, a list with the intersection as a member.
        Type::Nullable(inner) if matches!(**inner, Type::Intersection(_)) => Parts {
            members: vec![(**inner).clone()],
            nullable: true,
            intersection: false,
        },
        Type::Nullable(inner) => {
            let mut parts = parts(inner);
            parts.nullable = true;
            parts
        }
        Type::Union(members) => Parts {
            members: members
                .iter()
                .filter(|m| **m != Type::Null)
                .cloned()
                .collect(),
            nullable: members.contains(&Type::Null),
            intersection: false,
        },
        Type::Intersection(members) => Parts {
            members: members.to_vec(),
            nullable: false,
            intersection: true,
        },
        Type::Null => Parts {
            members: Vec::new(),
            nullable: true,
            intersection: false,
        },
        other => Parts {
            members: vec![other.clone()],
            nullable: false,
            intersection: false,
        },
    }
}

/// `get_type_kind`: a list is its list's kind, a union where an
/// intersection is among its members; `iterable` and `mixed` are named
/// for backwards compatibility however wide their mask is; anything else
/// with more than one member without `null` is a union.
fn kind_of(ty: &Type) -> Kind {
    let parts = parts(ty);
    if parts.intersection {
        return Kind::Intersection;
    }
    if parts
        .members
        .iter()
        .any(|member| matches!(member, Type::Intersection(_)))
    {
        return Kind::Union;
    }
    if matches!(ty, Type::Mixed) || parts.members.first() == Some(&Type::Iterable) {
        return Kind::Named;
    }
    if parts.members.len() > 1 {
        return Kind::Union;
    }
    Kind::Named
}

/// `reflection_type_factory`: the reflector of the type's kind with
/// the type on it. `legacy` is php's `legacy_behavior` argument, set
/// where the type is a parameter's, a return's or a property's and
/// clear for a member of a list.
pub(super) fn type_factory(
    req: &mut Request,
    ty: &Type,
    legacy: bool,
) -> Result<Owner<PhpObject>, Thrown> {
    let kind = kind_of(ty);
    let class = class_named(
        req,
        match kind {
            Kind::Named => b"ReflectionNamedType".as_slice(),
            Kind::Union => b"ReflectionUnionType".as_slice(),
            Kind::Intersection => b"ReflectionIntersectionType".as_slice(),
        },
    )?;
    let object = new_object(req, class)?;
    let is_mixed = matches!(ty, Type::Mixed);
    let is_only_null = matches!(ty, Type::Null);
    let legacy = legacy && kind == Kind::Named && !is_mixed && !is_only_null;
    let ty = ty.clone();
    super::keep_type(req, *object, TypeRef { ty, legacy })?;
    Ok(object)
}

/// `GET_REFLECTION_OBJECT_PTR` for a type reflector.
fn this_type(req: &mut Request, this: PhpObject) -> Result<TypeRef, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    match super::kept_type(req, this) {
        Some(reference) => Ok(reference),
        None => Err(throw_error(
            req,
            ce.error,
            "Internal error: Failed to retrieve the reflection object",
        )),
    }
}

/// `zend_named_reflection_type_to_string`: `zend_type_to_string` with
/// the `iterable` spelling kept where the type is the backwards
/// compatible `iterable` and not the `array|Traversable` it is a mask
/// for.
fn type_text(ty: &Type) -> String {
    type_to_string(ty)
}

/// `zend_type_to_string_without_null`: the same text with `null` out
/// of the mask, what `getName()` answers under `legacy_behavior`.
fn type_text_without_null(ty: &Type) -> String {
    let parts = parts(ty);
    if parts.intersection {
        return type_text(ty);
    }
    match parts.members.len() {
        0 => String::from("null"),
        1 => parts.members.first().map_or_else(String::new, type_text),
        _ => type_text(&Type::Union(parts.members.into())),
    }
}

// ------------------------------------------------ ReflectionParameter

/// The declaration of the function a `ParameterRef` names, looked up
/// again as every reflector here looks its subject up again.
fn function_decl_of(req: &mut Request, function: &FunctionRef) -> Option<FunctionDecl> {
    match function {
        FunctionRef::Closure(object) => closure_decl(req, *object),
        FunctionRef::Invoke(closure) => {
            super::closure_invoke(req, *closure).map(|method| method.decl.function)
        }
        FunctionRef::Global(name) => {
            let found = lookup_function(req, name)?;
            req.symbols().function_decl(found).cloned()
        }
        FunctionRef::Method(class, name) => {
            if let Some((property, kind)) = super::member::hook_name(name) {
                return php_ext::call::property_hook(req, *class, property, kind)
                    .map(|(_, decl)| decl.function);
            }
            let row = req.symbols().method_row(*class, name)?;
            req.symbols().row_method(row).map(|m| m.function.clone())
        }
    }
}

/// `param->fptr->common.scope`.
fn scope_of(req: &mut Request, function: &FunctionRef) -> Option<ClassId> {
    match function {
        FunctionRef::Global(_) => None,
        FunctionRef::Method(class, _) => Some(*class),
        // A closure declared in a method carries that class as its
        // scope, and `zend_get_closure_method_def` hands the
        // declaration back whatever the scope is, so the scope a
        // parameter's `declaringClass` reads is the closure's own.
        FunctionRef::Closure(object) => object.closure(req)?.scope,
        FunctionRef::Invoke(_) => lookup_class(req, b"Closure").ok().flatten(),
    }
}

/// The one class a type names, `ZEND_TYPE_HAS_NAME`: a name alone,
/// nullable or beside `MAY_BE_*` bits, as php stores it in `arg_info` beside
/// the mask; none for a type of bits alone, an intersection, or
/// two names, which php stores as a list.
fn one_name(ty: &Type) -> Option<&Type> {
    let is_name =
        |member: &&Type| matches!(member, Type::Named(_) | Type::SelfClass | Type::ParentClass);
    match ty {
        Type::Nullable(inner) => one_name(inner),
        Type::Union(members) => {
            let mut found = members.iter().filter(is_name);
            let first = found.next();
            if found.next().is_some() {
                return None;
            }
            first
        }
        Type::Named(_) | Type::SelfClass | Type::ParentClass => Some(ty),
        _ => None,
    }
}

/// `ZEND_TYPE_PURE_MASK_WITHOUT_NULL`: the `MAY_BE_*` bits of the type,
/// `null`'s aside.
fn mask_without_null(ty: &Type) -> u32 {
    ty.mask() & !Type::Null.mask()
}

/// `ZEND_TYPE_IS_ITERABLE_FALLBACK`: `iterable` written in the type.
fn has_iterable(ty: &Type) -> bool {
    match ty {
        Type::Iterable => true,
        Type::Nullable(inner) => has_iterable(inner),
        Type::Union(members) => members.iter().any(has_iterable),
        _ => false,
    }
}

/// `zend_get_closure_method_def`: the declaration behind a `Closure`,
/// which is the op array the `DECLARE_LAMBDA_FUNCTION` that made it
/// named; none for an object that is no closure.
pub(super) fn closure_decl(req: &mut Request, object: PhpObject) -> Option<FunctionDecl> {
    let function = object.closure(req)?.function;
    req.symbols().function_decl(function).cloned()
}

/// One `ReflectionParameter` over `offset` of `function`, php's
/// `reflection_parameter_factory`: `$name` written from the declared
/// parameter, the reference kept where php keeps `intern->ptr`, and
/// the closure the function's reflector holds kept as one more owner,
/// `ZVAL_OBJ_COPY(&intern->obj, ...)`, so the parameter outlives that
/// reflector and gives the closure up in `free_obj`.
///
/// ```php
/// class EC { public function __invoke(Throwable $exception): string { return 'x'; } }
/// function ctl() { return new EC; }
/// $r = new ReflectionFunction(ctl()(...));
/// $r = $r->getParameters()[0] ?? null;
/// var_dump($r->getName());
/// $p = (new ReflectionMethod(fn (int $n) => $n, '__invoke'))->getParameters()[0];
/// var_dump($p->getPosition(), (string) $p->getType());
/// ```
/// ```expect
/// string(9) "exception"
/// int(0)
/// string(3) "int"
/// ```
///
/// The owner is given up with the parameter, so what the closure binds
/// is destroyed then:
///
/// ```php
/// class G { function __destruct() { echo "gone\n"; } }
/// $o = new G; $c = function (int $x) use ($o) {}; unset($o);
/// $p = (new ReflectionFunction($c))->getParameters()[0]; unset($c);
/// echo "held\n"; unset($p); echo "given up\n";
/// ```
/// ```expect
/// held
/// gone
/// given up
/// ```
pub(super) fn parameter_factory(
    req: &mut Request,
    function: &FunctionRef,
    decl: &FunctionDecl,
    offset: usize,
) -> Result<Owner<PhpObject>, Thrown> {
    let class = class_named(req, b"ReflectionParameter")?;
    let object = new_object(req, class)?;
    let name = decl
        .parameters
        .get(offset)
        .map(|p| p.name.0.to_vec())
        .unwrap_or_default();
    set_name(req, *object, &name)?;
    if let Some(closure) = function.closure() {
        super::keep_closure(req, *object, closure);
    }
    let reference = ParameterRef {
        function: function.clone(),
        offset,
        required: offset < required_parameters(decl),
    };
    super::keep_parameter(req, *object, reference)?;
    Ok(object)
}

/// `GET_REFLECTION_OBJECT_PTR` for a parameter reflector, with the
/// declaration and the one parameter it points at.
fn this_parameter(
    req: &mut Request,
    this: PhpObject,
) -> Result<(ParameterRef, FunctionDecl, Parameter), Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let Some(reference) = super::kept_parameter(req, this) else {
        return Err(throw_error(
            req,
            ce.error,
            "Internal error: Failed to retrieve the reflection object",
        ));
    };
    let Some(decl) = function_decl_of(req, &reference.function) else {
        return Err(throw_error(
            req,
            ce.error,
            "Internal error: Failed to retrieve the reflection object",
        ));
    };
    let Some(parameter) = decl.parameters.get(reference.offset).cloned() else {
        return Err(throw_error(
            req,
            ce.error,
            "Internal error: Failed to retrieve the reflection object",
        ));
    };
    Ok((reference, decl, parameter))
}

/// `ReflectionParameter::__construct`, over the function and the
/// parameter the stub names: the function found as php finds it, a string
/// being `zend_fetch_function` with a leading `\` dropped and an array
/// being `array($class, $method)`, then the parameter by name or by
/// offset. A `Closure` is `zend_get_closure_method_def` over the
/// object, which the reflector keeps as php keeps `intern->obj`, so
/// the parameter it reflects is the closure's own; an object of any
/// other class is the argument error.
///
/// ```php
/// $p = new ReflectionParameter(function ($a, int $b = 2) {}, 0);
/// var_dump($p->getName(), $p->getPosition(), $p->isOptional());
/// $q = new ReflectionParameter(fn (int $n) => $n, 'n');
/// var_dump($q->getName(), $q->getType()->getName());
/// echo $p, "\n";
/// ```
/// ```expect
/// string(1) "a"
/// int(0)
/// bool(false)
/// string(1) "n"
/// string(3) "int"
/// Parameter #0 [ <required> $a ]
/// ```
///
/// ```php
/// $p = new ReflectionParameter('str_repeat', 1); var_dump($p->getName(), $p->getPosition());
/// ```
/// ```expect
/// string(5) "times"
/// int(1)
/// ```
///
/// ```php
/// $p = new ReflectionParameter(['Exception', '__construct'], 'code'); var_dump($p->getName(), $p->getPosition());
/// ```
/// ```expect
/// string(4) "code"
/// int(1)
/// ```
///
/// ```php
/// new ReflectionParameter('nope', 0);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Function nope() does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionParameter->__construct('nope', 0)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionParameter('str_repeat', 9);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: The parameter specified by its offset could not be found in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionParameter->__construct('str_repeat', 9)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionParameter('str_repeat', 'nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: The parameter specified by its name could not be found in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionParameter->__construct('str_repeat', 'nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
/// The function argument of `ReflectionParameter::__construct`,
/// php's `switch (Z_TYPE_P(reference))`: a string is
/// `zend_fetch_function` with a leading `\\` dropped, an array is
/// `array($class, $method)` with the object's class where the first
/// element is an object, an object is a closure, and anything else is
/// php's argument error.
///
/// ```php
/// class I { function __invoke($a) {} }
/// try { new ReflectionParameter(0, 0); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// try { new ReflectionParameter(new stdClass, 0); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// echo new ReflectionParameter(new I, 0), "\n";
/// ```
/// ```expect
/// ReflectionParameter::__construct(): Argument #1 ($function) must be a string, an array(class, method), or a callable object, int given
/// Method stdClass::__invoke() does not exist
/// Parameter #0 [ <required> $a ]
/// ```
fn construct_function(req: &mut Request, first: Value) -> Result<FunctionRef, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let function = match first.as_ref() {
        ValueRef::Array(array) => {
            let class = array.get(&Key::Int(0));
            let method = array.get(&Key::Int(1));
            let (Some(class), Some(method)) = (class, method) else {
                return Err(throw(
                    req,
                    ce.reflection_exception,
                    "Expected array($object, $method) or array($classname, $method)",
                ));
            };
            // `zval_try_get_string` over a class that is no object and
            // over the method, so a `Stringable` names either.
            let (class, object) = if let ValueRef::Object(object) = class.deref().as_ref() {
                (object.class(), Some(object))
            } else {
                let name = with_string(req, &class, <[u8]>::to_vec)?;
                (class_named(req, &name)?, None)
            };
            let method = with_string(req, &method, <[u8]>::to_vec)?;
            // An object's class is looked up with the closure it is, so
            // `[$closure, '__invoke']` reaches the method php makes.
            let found = if let Some(object) = object {
                super::method_or_invoke(req, class, &method, Some(object))
            } else {
                super::table_method(req, class, &method)
            };
            let Some(found) = found else {
                let class = class_name_of(req, class);
                return Err(throw_exception_ex(
                    req,
                    ce.reflection_exception,
                    &format!(
                        "Method {}::{}() does not exist",
                        String::from_utf8_lossy(&class),
                        String::from_utf8_lossy(&method)
                    ),
                ));
            };
            if let Some(super::HeldMethod::Invoke(function)) = found.held {
                FunctionRef::Invoke(function)
            } else {
                FunctionRef::Method(found.scope, found.decl.function.name.0.to_vec())
            }
        }
        // php's `IS_OBJECT` arm: `zend_get_closure_method_def` where
        // the object is a `Closure`, else its class's `__invoke` or
        // `Method C::__invoke() does not exist`.
        ValueRef::Object(object) => {
            if closure_decl(req, object).is_some() {
                FunctionRef::Closure(object)
            } else if let Some(found) = super::table_method(req, object.class(), b"__invoke") {
                FunctionRef::Method(found.scope, found.decl.function.name.0.to_vec())
            } else {
                let class = class_name_of(req, object.class());
                return Err(throw_exception_ex(
                    req,
                    ce.reflection_exception,
                    &format!(
                        "Method {}::__invoke() does not exist",
                        String::from_utf8_lossy(&class)
                    ),
                ));
            }
        }
        ValueRef::Str(_) => {
            let name = first.as_str_bytes().unwrap_or_default().to_vec();
            let Some(found) = php_ext::call::fetch_function(req, &name)? else {
                return Err(throw_exception_ex(
                    req,
                    ce.reflection_exception,
                    &format!(
                        "Function {}() does not exist",
                        String::from_utf8_lossy(&name)
                    ),
                ));
            };
            let declared = req
                .symbols()
                .function_decl(found)
                .map(|decl| decl.name.0.to_vec())
                .unwrap_or_default();
            FunctionRef::Global(declared)
        }
        // `default:` of php's `switch (Z_TYPE_P(reference))`,
        // `zend_argument_error` with `ReflectionException` as its class.
        _ => {
            let given = value_name(req, &first);
            return Err(php_ext::raising::argument_error(
                req,
                ce.reflection_exception,
                1,
                &[
                    b"must be a string, an array(class, method), or a callable object, ".as_slice(),
                    &given,
                    b" given",
                ]
                .concat(),
            ));
        }
    };
    Ok(function)
}

/// The parameter argument of `ReflectionParameter::__construct`,
/// php's `Z_PARAM_STR_OR_LONG`: the offset of the parameter of that
/// name, or the offset itself, held to the declared count.
fn construct_offset(
    req: &mut Request,
    decl: &FunctionDecl,
    param: Union<i64, PhpString>,
) -> Result<usize, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let offset = match param {
        Union::B(wanted) => {
            let found = decl
                .parameters
                .iter()
                .position(|p| p.name.0.as_ref() == wanted.as_bytes());
            match found {
                Some(offset) => offset,
                None => {
                    return Err(throw(
                        req,
                        ce.reflection_exception,
                        "The parameter specified by its name could not be found",
                    ));
                }
            }
        }
        Union::A(position) => {
            if position < 0 {
                return Err(php_ext::raising::argument_error(
                    req,
                    ce.value_error,
                    2,
                    "must be greater than or equal to 0",
                ));
            }
            let position = usize::try_from(position).unwrap_or(usize::MAX);
            if position >= decl.parameters.len() {
                return Err(throw(
                    req,
                    ce.reflection_exception,
                    "The parameter specified by its offset could not be found",
                ));
            }
            position
        }
    };
    Ok(offset)
}

/// The default expression php's `get_parameter_default` would hand
/// back: the declared one, and none for the stub's `UNKNOWN`, which
/// is an optional parameter with nothing to show.
fn parameter_default(parameter: &Parameter) -> Option<&ConstExpr> {
    parameter.default.expr()
}

/// php's `reflection_exception` for a parameter whose default
/// `get_parameter_default` could not read.
fn no_default(req: &mut Request) -> Thrown {
    let ce = match crate::throwables::Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    throw_exception_ex(
        req,
        ce.reflection_exception,
        "Internal error: Failed to retrieve the default value",
    )
}

// --------------------------------------- ReflectionFunctionAbstract

/// The function a `ReflectionFunctionAbstract` reflects, php's
/// `GET_REFLECTION_OBJECT_PTR(fptr)`: `$class` and `$name` for a
/// `ReflectionMethod`, `$name` alone for a `ReflectionFunction`,
/// since the reflector keeps the names and not php's `intern->ptr`.
/// Every accessor of a reflector over a closure reads that
/// declaration, so what a closure was written as is what they answer:
/// its parameters, its site, its short name, and the class its
/// parameters name as the one that declared them.
///
/// ```php
/// class C { public $p = 1; function m() { return function ($x) { return $this->p + $x; }; } }
/// $f = (new C)->m();
/// $r = new ReflectionFunction($f);
/// var_dump($r->getNumberOfParameters(), $r->isStatic(), $r->isUserDefined(), $r->isInternal(), $r->inNamespace(), $r->getShortName());
/// var_dump($r->getStartLine(), $r->getParameters());
/// $p = new ReflectionParameter($f, 0);
/// var_dump($p->getDeclaringFunction()->isClosure(), $p->getDeclaringClass()->getName(), $p->isDefaultValueAvailable());
/// ```
/// ```expect
/// int(1)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// string(18) "{closure:C::m():2}"
/// int(2)
/// array(1) {
///   [0]=>
///   object(ReflectionParameter)#4 (1) {
///     ["name"]=>
///     string(1) "x"
///   }
/// }
/// bool(true)
/// string(1) "C"
/// bool(false)
/// ```
pub(super) fn this_function(
    req: &mut Request,
    this: PhpObject,
) -> Result<(FunctionRef, FunctionDecl), Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let name = this_name(req, this, "ReflectionFunctionAbstract")?;
    let scope = this
        .prop(req, b"class")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
    let kept = super::kept_closure(req, this);
    // `intern->obj`, which a reflector over a closure keeps: php reads
    // the declaration back off it with `zend_get_closure_method_def`,
    // since the `{closure:…}` name is in no table to look up. A method
    // reflector keeps one for a closure with a scope, whose parameter's
    // `getDeclaringFunction` made it.
    if let Some(closure) = kept
        && let Some(decl) = closure_decl(req, closure)
        && (scope.is_none() || decl.name.0.as_ref() == name.as_slice())
    {
        return Ok((FunctionRef::Closure(closure), decl));
    }
    // `intern->ptr`, the method a `ReflectionMethod`'s constructor or
    // factory found, read as php reads it: no lookup by name again.
    match super::kept_method(req, this) {
        Some(super::HeldMethod::Row(row)) => {
            if let Some(method) = req.symbols().row_method(row) {
                let decl = method.function.clone();
                return Ok((FunctionRef::Method(row.scope(), decl.name.0.to_vec()), decl));
            }
        }
        Some(super::HeldMethod::Invoke(function)) => {
            if let Some(invoke) = super::closure_invoke(req, function) {
                return Ok((FunctionRef::Invoke(function), invoke.decl.function));
            }
        }
        None => {}
    }
    let function = if let Some(scope) = scope {
        {
            let class = class_named(req, &scope)?;
            // A hook's reflector names the hook, `$name::get`, which no
            // method table holds.
            if let Some((property, kind)) = super::member::hook_name(&name)
                && let Some((scope, decl)) =
                    php_ext::call::property_hook(req, class, property, kind)
            {
                return Ok((
                    FunctionRef::Method(scope, decl.function.name.0.to_vec()),
                    decl.function,
                ));
            }
            let Some(found) = super::method_or_invoke(req, class, &name, kept) else {
                let class = class_name_of(req, class);
                return Err(throw_exception_ex(
                    req,
                    ce.reflection_exception,
                    &format!(
                        "Method {}::{}() does not exist",
                        String::from_utf8_lossy(&class),
                        String::from_utf8_lossy(&name)
                    ),
                ));
            };
            if let Some(super::HeldMethod::Invoke(function)) = found.held {
                FunctionRef::Invoke(function)
            } else {
                FunctionRef::Method(found.scope, found.decl.function.name.0.to_vec())
            }
        }
    } else {
        {
            if lookup_function(req, &name).is_none() {
                return Err(throw_exception_ex(
                    req,
                    ce.reflection_exception,
                    &format!(
                        "Function {}() does not exist",
                        String::from_utf8_lossy(&name)
                    ),
                ));
            }
            FunctionRef::Global(name)
        }
    };
    let Some(decl) = function_decl_of(req, &function) else {
        return Err(throw_error(
            req,
            ce.error,
            "Internal error: Failed to retrieve the reflection object",
        ));
    };
    Ok((function, decl))
}

/// `ReflectionParameter` as the stub declares it, with the
/// `parameter_reference` its constructor and
/// `reflection_parameter_factory` lay down beside it, given back with it:
///
/// ```php
/// function f(int $a) {}
/// $w = new ReflectionParameter('f', 0);
/// $m = memory_get_usage(); $r = new ReflectionParameter('f', 0); $n = memory_get_usage(); unset($r);
/// var_dump($n - $m, memory_get_usage() - $n);
/// ```
/// ```expect
/// int(120)
/// int(-120)
/// ```
#[php_class(implements(crate::Reflector), not_serializable, size = 96, payload = crate::Reflected, handlers = crate::HANDLERS)]
pub(crate) struct ReflectionParameter;

#[php_class]
impl ReflectionParameter {
    #[php(property)]
    const name: PhpString;

    #[php(private, implementation = crate::func::ReflectionFunctionAbstract::__clone)]
    fn __clone(this: This<Self>);

    /// `ReflectionParameter::__construct($function, int|string $param)`:
    /// the function [`construct_function`] reads off the first argument,
    /// and the parameter at the offset or of the name the second gives.
    ///
    /// ```php
    /// function f($a, $b) {}
    /// echo new ReflectionParameter('f', 1), "\n", new ReflectionParameter('f', 'a'), "\n";
    /// try { new ReflectionParameter('f', 5); } catch (ReflectionException $x) { echo $x->getMessage(), "\n"; }
    /// try { new ReflectionParameter('f', 'z'); } catch (ReflectionException $x) { echo $x->getMessage(), "\n"; }
    /// try { new ReflectionParameter('f', -1); } catch (ValueError $x) { echo $x->getMessage(), "\n"; }
    /// try { new ReflectionParameter('f', []); } catch (TypeError $x) { echo $x->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Parameter #1 [ <required> $b ]
    /// Parameter #0 [ <required> $a ]
    /// The parameter specified by its offset could not be found
    /// The parameter specified by its name could not be found
    /// ReflectionParameter::__construct(): Argument #2 ($param) must be greater than or equal to 0
    /// ReflectionParameter::__construct(): Argument #2 ($param) must be of type string|int, array given
    /// ```
    ///
    /// An array's class and method are cast to string as php's
    /// `zval_try_get_string` casts them, a `Stringable` among them:
    ///
    /// ```php
    /// class A { function m($p) {} }
    /// $s = new class { public function __toString() { return 'm'; } };
    /// echo new ReflectionParameter([A::class, $s], 'p'), "\n";
    /// try { new ReflectionParameter([A::class, new class { public function __toString() { return 'nope'; } }], 'p'); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Parameter #0 [ <required> $p ]
    /// Method A::nope() does not exist
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        function: Untyped<Value>,
        param: Union<i64, PhpString>,
    ) -> Result<(), Thrown> {
        let ce = crate::throwables::Throwables::of(req)?;
        let function = construct_function(req, function.0)?;
        let Some(decl) = function_decl_of(req, &function) else {
            return Err(throw_error(
                req,
                ce.error,
                "Internal error: Failed to retrieve the reflection object",
            ));
        };
        let offset = construct_offset(req, &decl, param)?;
        let name = decl
            .parameters
            .get(offset)
            .map(|p| p.name.0.to_vec())
            .unwrap_or_default();
        let reference = ParameterRef {
            function,
            offset,
            required: offset < required_parameters(&decl),
        };
        let this = this.object();
        set_name(req, this, &name)?;
        // `intern->obj`: a parameter of a closure keeps the closure, as
        // php's does, so the row's `FunctionRef::Closure` names an
        // object that stands for as long as the reflector does.
        if let Some(closure) = reference.function.closure() {
            super::keep_closure(req, this, closure);
        }
        super::keep_parameter(req, this, reference)
    }

    /// `ReflectionParameter::__toString()`: `_parameter_string` over the
    /// one parameter, indent empty, the same row `__toString` of the
    /// function prints inside its `Parameters` block.
    ///
    /// ```php
    /// echo new ReflectionParameter('str_repeat', 1), "\n";
    /// ```
    /// ```expect
    /// Parameter #1 [ <required> int $times ]
    /// ```
    ///
    /// ```php
    /// function f($a, $b = null) {}
    /// echo new ReflectionParameter('f', 1), "\n";
    /// ```
    /// ```expect
    /// Parameter #1 [ <optional> $b = NULL ]
    /// ```
    ///
    /// A native's default is the text `gen_stub.php` printed from the
    /// stub's expression, a string double-quoted with its escapes, and
    /// `<default>` for the stub's `UNKNOWN`, which reads as optional with no
    /// value to fetch (#377):
    ///
    /// ```php
    /// echo new ReflectionParameter('trim', 1), "\n", new ReflectionParameter('str_getcsv', 3), "\n", new ReflectionParameter('get_class', 0), "\n";
    /// var_dump((new ReflectionParameter('get_class', 0))->isOptional(), (new ReflectionParameter('get_class', 0))->isDefaultValueAvailable());
    /// ```
    /// ```expect
    /// Parameter #1 [ <optional> string $characters = " \n\r\t\v\x00" ]
    /// Parameter #3 [ <optional> string $escape = "\\" ]
    /// Parameter #0 [ <optional> object $object = <default> ]
    /// bool(true)
    /// bool(false)
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let (reference, decl, parameter) = this_parameter(req, this.object())?;
        let internal = decl.file.0.is_empty();
        let text = parameter_string(
            req,
            &parameter,
            reference.offset,
            reference.required,
            internal,
        );
        PhpString::new(req, text.as_bytes())
    }

    /// `ReflectionParameter::getName()`: `arg_info->name`.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter('str_repeat', 0))->getName());
    /// ```
    /// ```expect
    /// string(6) "string"
    /// ```
    #[php(tentative)]
    fn getName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let (_, _, parameter) = this_parameter(req, this.object())?;
        let name = parameter.name.0.to_vec();
        PhpString::new(req, &name)
    }

    /// `ReflectionParameter::isPassedByReference()`:
    /// `ZEND_ARG_SEND_MODE`, which is set for `ZEND_SEND_BY_REF` and for
    /// `ZEND_SEND_PREFER_REF`. The declaration model carries the `&` of
    /// the declaration and not php's third way of sending, so a
    /// `prefer-ref` parameter answers as php's by-value one does (#413).
    ///
    /// ```php
    /// var_dump((new ReflectionParameter('str_repeat', 0))->isPassedByReference());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isPassedByReference(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, parameter) = this_parameter(req, this.object())?;
        Ok(parameter.by_ref)
    }

    /// `ReflectionParameter::canBePassedByValue()`: the way the parameter
    /// is sent is not `ZEND_SEND_BY_REF`, which `ZEND_SEND_PREFER_REF`
    /// is not either.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter('str_repeat', 0))->canBePassedByValue(), (new ReflectionParameter('sort', 0))->canBePassedByValue(), (new ReflectionParameter('array_multisort', 0))->canBePassedByValue(), (new ReflectionParameter('array_multisort', 0))->isPassedByReference());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn canBePassedByValue(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, parameter) = this_parameter(req, this.object())?;
        Ok(!parameter.by_ref || parameter.prefer_ref)
    }

    /// `ReflectionParameter::getDeclaringFunction()`:
    /// `reflection_function_factory` for a function, and
    /// `reflection_method_factory` with `common.scope` for a method.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter("str_repeat", 0))->getDeclaringFunction());
    /// ```
    /// ```expect
    /// object(ReflectionFunction)#2 (1) {
    ///   ["name"]=>
    ///   string(10) "str_repeat"
    /// }
    /// ```
    ///
    /// A closure with a scope is reflected as a method of it, over the
    /// closure:
    ///
    /// ```php
    /// class A {} $c = (function ($x) {})->bindTo(new A, A::class);
    /// $m = (new ReflectionFunction($c))->getParameters()[0]->getDeclaringFunction();
    /// var_dump(get_class($m), $m->class, $m->isClosure(), $m->getNumberOfParameters());
    /// ```
    /// ```expect
    /// string(16) "ReflectionMethod"
    /// string(1) "A"
    /// bool(true)
    /// int(1)
    /// ```
    #[php(tentative)]
    fn getDeclaringFunction(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<crate::func::ReflectionFunctionAbstract>>, Thrown> {
        let (reference, decl, _) = this_parameter(req, this.object())?;
        let object = match &reference.function {
            FunctionRef::Global(_) => {
                let class = class_named(req, b"ReflectionFunction")?;
                let object = new_object(req, class)?;
                set_name(req, *object, &decl.name.0)?;
                object
            }
            // php's `reflection_function_factory` over the closure itself,
            // which is `intern->obj` again, so the reflector it hands back
            // reflects the closure and not a name no table holds; and
            // `reflection_method_factory` where the closure has a scope,
            // `$class` the scope's name.
            FunctionRef::Closure(closure) => {
                let closure = *closure;
                let scope = closure.closure(req).and_then(|info| info.scope);
                let reflector = if scope.is_some() {
                    b"ReflectionMethod".as_slice()
                } else {
                    b"ReflectionFunction".as_slice()
                };
                let class = class_named(req, reflector)?;
                let object = new_object(req, class)?;
                set_name(req, *object, &decl.name.0)?;
                if let Some(scope) = scope {
                    let named = class_name_of(req, scope);
                    set_string_prop(req, *object, b"class", &named)?;
                    super::keep(req, *object, scope, Some(closure));
                } else {
                    super::keep_closure(req, *object, closure);
                }
                object
            }
            FunctionRef::Method(scope, name) => {
                let class = class_named(req, b"ReflectionMethod")?;
                let object = new_object(req, class)?;
                let scope = class_name_of(req, *scope);
                set_name(req, *object, name)?;
                set_string_prop(req, *object, b"class", &scope)?;
                object
            }
            // `reflection_method_factory` over the `__invoke` the parameter
            // belongs to, the trampoline over the closure's function.
            FunctionRef::Invoke(function) => {
                let function = *function;
                let class = class_named(req, b"ReflectionMethod")?;
                let object = new_object(req, class)?;
                set_name(req, *object, b"__invoke")?;
                set_string_prop(req, *object, b"class", b"Closure")?;
                let scope = class_named(req, b"Closure")?;
                super::keep(req, *object, scope, None);
                super::keep_method(req, *object, Some(super::HeldMethod::Invoke(function)));
                object
            }
        };
        Ok(object.into_instance())
    }

    /// `ReflectionParameter::getDeclaringClass()`:
    /// `zend_reflection_class_factory` over `common.scope`, and `null`
    /// for a function outside a class.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter(["Exception","__construct"], 0))->getDeclaringClass());
    /// var_dump((new ReflectionParameter("str_repeat", 0))->getDeclaringClass());
    /// ```
    /// ```expect
    /// object(ReflectionClass)#2 (1) {
    ///   ["name"]=>
    ///   string(9) "Exception"
    /// }
    /// NULL
    /// ```
    #[php(tentative)]
    fn getDeclaringClass(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::class::ReflectionClass>>>, Thrown> {
        let (reference, _, _) = this_parameter(req, this.object())?;
        let Some(scope) = scope_of(req, &reference.function) else {
            return Ok(None);
        };
        let object = reflection_class_factory(req, scope)?;
        Ok(Some(object.into_instance()))
    }

    /// `ReflectionParameter::getClass()`: where the declared type names
    /// one class (`ZEND_TYPE_HAS_NAME`: alone, nullable, or beside
    /// `MAY_BE_*` bits), that class, `self` and `parent` read off the function's
    /// scope and any other name looked up with the autoloader; `null`
    /// for a type with no name or with several.
    ///
    /// ```php
    /// class P {} class C extends P { function f(self $a, ?parent $b, int|ArrayObject $c, P|C $d, int $e, $f) {} }
    /// foreach ((new ReflectionMethod('C', 'f'))->getParameters() as $p) { $c = @$p->getClass(); echo $c ? $c->name : 'null', "\n"; }
    /// $f = function (Missing $m) {};
    /// try { @(new ReflectionFunction($f))->getParameters()[0]->getClass(); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// $f = function (self $s) {};
    /// try { (new ReflectionFunction($f))->getParameters()[0]->getClass(); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// $f = (function (parent $s) {})->bindTo(null, P::class);
    /// try { @(new ReflectionFunction($f))->getParameters()[0]->getClass(); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// C
    /// P
    /// ArrayObject
    /// null
    /// null
    /// null
    /// Class "Missing" does not exist
    ///
    /// Deprecated: Method ReflectionParameter::getClass() is deprecated since 8.0, use ReflectionParameter::getType() instead in %s on line 7
    /// Parameter uses "self" as type but function is not a class member
    /// Parameter uses "parent" as type although class does not have a parent
    /// ```
    #[php(
        tentative,
        deprecated(since = "8.0", message = "use ReflectionParameter::getType() instead")
    )]
    fn getClass(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::class::ReflectionClass>>>, Thrown> {
        let ce = crate::throwables::Throwables::of(req)?;
        let (reference, _, parameter) = this_parameter(req, this.object())?;
        let Some(name) = parameter.ty.as_ref().and_then(one_name) else {
            return Ok(None);
        };
        let scope = scope_of(req, &reference.function);
        let class = match name {
            Type::SelfClass => scope.ok_or_else(|| {
                "Parameter uses \"self\" as type but function is not a class member".to_owned()
            }),
            Type::ParentClass => match scope {
                None => Err(
                    "Parameter uses \"parent\" as type but function is not a class member"
                        .to_owned(),
                ),
                Some(scope) => class_chain(req, scope).get(1).copied().ok_or_else(|| {
                    "Parameter uses \"parent\" as type although class does not have a parent"
                        .to_owned()
                }),
            },
            Type::Named(name) => autoload(req, &name.0)?.ok_or_else(|| {
                format!(
                    "Class \"{}\" does not exist",
                    String::from_utf8_lossy(&name.0)
                )
            }),
            _ => return Ok(None),
        };
        match class {
            Ok(class) => Ok(Some(reflection_class_factory(req, class)?.into_instance())),
            Err(message) => Err(throw_exception_ex(req, ce.reflection_exception, &message)),
        }
    }

    /// `ReflectionParameter::hasType()`: `ZEND_TYPE_IS_SET`.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter("json_decode", 0))->hasType());
    /// var_dump((new ReflectionParameter(["ReflectionParameter","__construct"], 0))->hasType());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn hasType(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, parameter) = this_parameter(req, this.object())?;
        Ok(parameter.ty.is_some())
    }

    /// `ReflectionParameter::getType()`: `reflection_type_factory` over
    /// the declared type with `legacy_behavior` set, `null` for a
    /// parameter with no declared type.
    ///
    /// ```php
    /// echo (new ReflectionParameter("str_repeat", 0))->getType(), "\n";
    /// var_dump((new ReflectionParameter(["ReflectionParameter","__construct"], 0))->getType());
    /// ```
    /// ```expect
    /// string
    /// NULL
    /// ```
    #[php(tentative)]
    fn getType(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<ReflectionType>>>, Thrown> {
        let (_, _, parameter) = this_parameter(req, this.object())?;
        let Some(ty) = parameter.ty else {
            return Ok(None);
        };
        let object = type_factory(req, &ty, true)?;
        Ok(Some(object.into_instance()))
    }

    /// `ReflectionParameter::isArray()`: the `MAY_BE_*` mask, `null` aside,
    /// is `array` alone, and `false` for `iterable`, which php 8.2 compiles
    /// to `Traversable|array` and marks for this answer.
    ///
    /// ```php
    /// function f(array $a, ?array $b, iterable $c, array|string $d, array|ArrayObject $e, $f) {}
    /// foreach ((new ReflectionFunction('f'))->getParameters() as $p) { var_dump(@$p->isArray()); }
    /// var_dump((new ReflectionParameter('f', 0))->isArray());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    ///
    /// Deprecated: Method ReflectionParameter::isArray() is deprecated since 8.0, use ReflectionParameter::getType() instead in %s on line 4
    /// bool(true)
    /// ```
    #[php(
        tentative,
        deprecated(since = "8.0", message = "use ReflectionParameter::getType() instead")
    )]
    fn isArray(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, parameter) = this_parameter(req, this.object())?;
        Ok(parameter
            .ty
            .as_ref()
            .is_some_and(|ty| !has_iterable(ty) && mask_without_null(ty) == Type::Array.mask()))
    }

    /// `ReflectionParameter::isCallable()`: the `MAY_BE_*` mask, `null`
    /// aside, is `callable` alone.
    ///
    /// ```php
    /// function f(callable $a, ?callable $b, callable|string $c, Closure $d, $e) {}
    /// foreach ((new ReflectionFunction('f'))->getParameters() as $p) { var_dump(@$p->isCallable()); }
    /// var_dump((new ReflectionParameter('f', 0))->isCallable());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    ///
    /// Deprecated: Method ReflectionParameter::isCallable() is deprecated since 8.0, use ReflectionParameter::getType() instead in %s on line 4
    /// bool(true)
    /// ```
    #[php(
        tentative,
        deprecated(since = "8.0", message = "use ReflectionParameter::getType() instead")
    )]
    fn isCallable(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, parameter) = this_parameter(req, this.object())?;
        Ok(parameter
            .ty
            .as_ref()
            .is_some_and(|ty| mask_without_null(ty) == Type::Callable.mask()))
    }

    /// `ReflectionParameter::allowsNull()`: no declared type, or a mask
    /// with `null` in it.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter("str_repeat", 0))->allowsNull());
    /// var_dump((new ReflectionParameter("json_decode", 1))->allowsNull());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn allowsNull(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, parameter) = this_parameter(req, this.object())?;
        let allows = match &parameter.ty {
            None | Some(Type::Mixed) => true,
            Some(ty) => parts(ty).nullable,
        };
        Ok(allows)
    }

    /// `ReflectionParameter::getPosition()`: `param->offset`.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter('str_repeat', 1))->getPosition());
    /// ```
    /// ```expect
    /// int(1)
    /// ```
    #[php(tentative)]
    fn getPosition(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let (reference, _, _) = this_parameter(req, this.object())?;
        Ok(i64::try_from(reference.offset).unwrap_or(0))
    }

    /// `ReflectionParameter::isOptional()`: `!param->required`.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter("json_decode", 0))->isOptional());
    /// var_dump((new ReflectionParameter("json_decode", 1))->isOptional());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isOptional(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (reference, _, _) = this_parameter(req, this.object())?;
        Ok(!reference.required)
    }

    /// `ReflectionParameter::isDefaultValueAvailable()`: an internal
    /// parameter php's `zend_internal_arg_info` gives a `default_value`, a
    /// user one with a `RECV_INIT`; the stub's `UNKNOWN` is php's absent
    /// `default_value`.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter("json_encode", 0))->isDefaultValueAvailable());
    /// var_dump((new ReflectionParameter("json_encode", 1))->isDefaultValueAvailable());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isDefaultValueAvailable(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, parameter) = this_parameter(req, this.object())?;
        Ok(parameter_default(&parameter).is_some())
    }

    /// `ReflectionParameter::getDefaultValue()`: the default expression
    /// evaluated in the function's scope, `zval_update_constant_ex`.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter("json_encode", 1))->getDefaultValue());
    /// ```
    /// ```expect
    /// int(0)
    /// ```
    ///
    /// A user function's default is its `RECV_INIT` constant
    /// (`get_default_from_recv`), so a string is the one the compiler
    /// interned and an array the unit's; a native's is its stub's text
    /// evaluated afresh (`zend_get_default_from_internal_arg_info`):
    ///
    /// ```php
    /// function pd($x = 'pdefault', $z = [PHP_INT_SIZE]) {}
    /// foreach ((new ReflectionFunction('pd'))->getParameters() as $p) debug_zval_dump($p->getDefaultValue());
    /// debug_zval_dump((new ReflectionParameter(['DateTime', '__construct'], 0))->getDefaultValue());
    /// ```
    /// ```expect
    /// string(8) "pdefault" interned
    /// array(1) packed refcount(1){
    ///   [0]=>
    ///   int(8)
    /// }
    /// string(3) "now" refcount(1)
    /// ```
    ///
    /// An array the compiler folded is the unit's, which takes no owner:
    ///
    /// ```php
    /// function pa($y = ['p'], $w = []) {}
    /// foreach ((new ReflectionFunction('pa'))->getParameters() as $p) debug_zval_dump($p->getDefaultValue());
    /// ```
    /// ```expect regime=immutable-literals
    /// array(1) packed interned {
    ///   [0]=>
    ///   string(1) "p" interned
    /// }
    /// array(0) interned {
    /// }
    /// ```
    ///
    /// ```php
    /// (new ReflectionParameter("json_encode", 0))->getDefaultValue();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Internal error: Failed to retrieve the default value in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionParameter->getDefaultValue()
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    #[php(tentative)]
    fn getDefaultValue(req: &mut Request, this: This<Self>) -> Result<Owner<Value>, Thrown> {
        let (reference, decl, parameter) = this_parameter(req, this.object())?;
        let Some(expr) = parameter_default(&parameter).cloned() else {
            return Err(no_default(req));
        };
        let scope = scope_of(req, &reference.function);
        if super::func::is_internal(&decl) {
            eval_const_expr(req, &expr, scope)
        } else {
            eval_declared(req, &expr, scope)
        }
    }

    /// `ReflectionParameter::isDefaultValueConstant()`: whether the
    /// default is still a constant or a class constant after the compiler
    /// folded what it could.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter("json_encode", 1))->isDefaultValueConstant());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isDefaultValueConstant(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, parameter) = this_parameter(req, this.object())?;
        let Some(expr) = parameter_default(&parameter) else {
            return Err(no_default(req));
        };
        Ok(matches!(
            expr,
            ConstExpr::Constant { .. } | ConstExpr::ClassConstant { .. }
        ))
    }

    /// `ReflectionParameter::getDefaultValueConstantName()`: the
    /// constant's name, `Class::NAME` for a class constant, and `null`
    /// where the default is not one.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter("json_encode", 1))->getDefaultValueConstantName());
    /// ```
    /// ```expect
    /// NULL
    /// ```
    #[php(tentative)]
    fn getDefaultValueConstantName(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        let (_, _, parameter) = this_parameter(req, this.object())?;
        let Some(expr) = parameter_default(&parameter).cloned() else {
            return Err(no_default(req));
        };
        match expr {
            ConstExpr::Constant { name, .. } => PhpString::new(req, &name.resolved().0).map(Some),
            ConstExpr::ClassConstant { class, name, .. } => {
                let text = format!(
                    "{}::{}",
                    String::from_utf8_lossy(&class.0),
                    String::from_utf8_lossy(&name.0)
                );
                PhpString::new(req, text.as_bytes()).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// `ReflectionParameter::isVariadic()`: `ZEND_ARG_IS_VARIADIC`.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter(["ReflectionClass","newInstance"], 0))->isVariadic());
    /// var_dump((new ReflectionParameter("str_repeat", 0))->isVariadic());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isVariadic(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, parameter) = this_parameter(req, this.object())?;
        Ok(parameter.variadic)
    }

    /// `ReflectionParameter::isPromoted()`: `ZEND_ARG_IS_PROMOTED`, the
    /// bit constructor promotion sets on the parameter.
    ///
    /// ```php
    /// class P { function __construct(public int $x, $y) {} }
    /// var_dump((new ReflectionParameter(['P', '__construct'], 0))->isPromoted(), (new ReflectionParameter(['P', '__construct'], 1))->isPromoted(), (new ReflectionParameter('str_repeat', 0))->isPromoted());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// ```
    fn isPromoted(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, parameter) = this_parameter(req, this.object())?;
        Ok(parameter.promoted)
    }

    /// `ReflectionParameter::getAttributes(?string $name = null, int
    /// $flags = 0)`: `reflect_attributes` over the function's attributes
    /// at this offset. `ReflectionAttribute` is declared and not served (#414),
    /// so a parameter that carries one is php's `ReflectionException` for
    /// the missing class rather than a wrong list, and one that carries
    /// none is the empty array php gives it.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter("str_repeat", 0))->getAttributes());
    /// ```
    /// ```expect
    /// array(0) {
    /// }
    /// ```
    fn getAttributes(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] name: Option<PhpString>,
        #[php(default = 0)] flags: i64,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let (reference, decl, parameter) = this_parameter(req, this.object())?;
        let file = super::attr::declared_in(&decl.file.0);
        let scope = match reference.function {
            FunctionRef::Method(scope, _) => Some(scope),
            FunctionRef::Invoke(_) => lookup_class(req, b"Closure").ok().flatten(),
            FunctionRef::Closure(closure) => closure.closure(req).and_then(|info| info.scope),
            FunctionRef::Global(_) => None,
        };
        let attributes = parameter.attributes.clone();
        super::attr::reflect_attributes(
            req,
            &attributes,
            super::attr::Subject::new(scope, super::attr::TARGET_PARAMETER, file.as_deref()),
            name,
            flags,
        )
    }
}

/// `ReflectionType` as the stub declares it.
#[php_class(abstract, implements(crate::Stringable), not_serializable, size = 96)]
pub(crate) struct ReflectionType;

#[php_class]
impl ReflectionType {
    #[php(private, implementation = crate::func::ReflectionFunctionAbstract::__clone)]
    fn __clone(this: This<Self>);

    /// `ReflectionType::allowsNull()`: `ZEND_TYPE_ALLOW_NULL`, the `null`
    /// bit of the mask, which `?bool` carries and `string` does not.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter("str_repeat", 0))->getType()->allowsNull());
    /// var_dump((new ReflectionParameter("json_decode", 1))->getType()->allowsNull());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn allowsNull(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let reference = this_type(req, this.object())?;
        let allows = matches!(reference.ty, Type::Mixed) || parts(&reference.ty).nullable;
        Ok(allows)
    }

    /// `ReflectionType::__toString()`:
    /// `zend_named_reflection_type_to_string`, the type with its `null`
    /// where it has one, so `?bool` and not `bool`.
    ///
    /// ```php
    /// echo (new ReflectionParameter("json_decode", 1))->getType(), "\n";
    /// ```
    /// ```expect
    /// ?bool
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let reference = this_type(req, this.object())?;
        let text = type_text(&reference.ty);
        PhpString::new(req, text.as_bytes())
    }
}

/// `ReflectionNamedType` as the stub declares it, with the
/// `type_reference` `reflection_type_factory` lays down beside it, given
/// back with it, as beside a union and an intersection type:
///
/// ```php
/// function f(int $a) {}
/// $p = new ReflectionParameter('f', 0); $w = $p->getType();
/// $m = memory_get_usage(); $r = $p->getType(); $n = memory_get_usage(); unset($r);
/// var_dump($n - $m, memory_get_usage() - $n);
/// ```
/// ```expect
/// int(104)
/// int(-104)
/// ```
#[php_class(extends(ReflectionType), size = 96, payload = crate::Reflected, handlers = crate::HANDLERS)]
pub(crate) struct ReflectionNamedType;

#[php_class]
impl ReflectionNamedType {
    /// `ReflectionNamedType::getName()`: the type's text, with the `null`
    /// dropped under `legacy_behavior`, which is how `?bool` names itself
    /// `bool` while `__toString()` keeps the `?`.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter("json_decode", 1))->getType()->getName());
    /// echo (new ReflectionParameter("json_decode", 1))->getType(), "\n";
    /// ```
    /// ```expect
    /// string(4) "bool"
    /// ?bool
    /// ```
    #[php(tentative)]
    fn getName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let reference = this_type(req, this.object())?;
        let text = if reference.legacy {
            type_text_without_null(&reference.ty)
        } else {
            type_text(&reference.ty)
        };
        PhpString::new(req, text.as_bytes())
    }

    /// `ReflectionNamedType::isBuiltin()`: whether the type is only a
    /// mask, no class name in it; `static` is a class type for reflection
    /// and `iterable` answers `true`.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter("str_repeat", 0))->getType()->isBuiltin());
    /// var_dump((new ReflectionParameter(["Exception","__construct"], 2))->getType()->isBuiltin());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isBuiltin(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let reference = this_type(req, this.object())?;
        let named = parts(&reference.ty).members.iter().any(|member| {
            matches!(
                member,
                Type::Named(_) | Type::SelfClass | Type::ParentClass | Type::CalledClass
            )
        });
        Ok(!named)
    }
}

/// `ReflectionUnionType` as the stub declares it.
#[php_class(extends(ReflectionType), size = 96, payload = crate::Reflected, handlers = crate::HANDLERS)]
pub(crate) struct ReflectionUnionType;

#[php_class]
impl ReflectionUnionType {
    /// `ReflectionUnionType::getTypes()` and
    /// `ReflectionIntersectionType::getTypes()`: one reflector per member,
    /// `legacy_behavior` clear on each, in `append_type`'s order for a
    /// union: the class names as the list has them, then `static`,
    /// `callable`, `object`, `array`, `string`, `int`, `float`, `bool`,
    /// `null`.
    ///
    /// ```php
    /// var_dump((new ReflectionParameter(["ReflectionMethod","__construct"], 0))->getType()->getTypes());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   object(ReflectionNamedType)#1 (0) {
    ///   }
    ///   [1]=>
    ///   object(ReflectionNamedType)#3 (0) {
    ///   }
    /// }
    /// ```
    ///
    /// An intersection among a union's members makes a DNF union, and so
    /// does an intersection made nullable by a `null` default:
    ///
    /// ```php
    /// interface X {} interface Y {}
    /// function f((X&Y)|null $a, X&Y $b = null, X&Y $c) {}
    /// foreach ((new ReflectionFunction('f'))->getParameters() as $p) {
    ///     $t = $p->getType();
    ///     echo $t, ' ', get_class($t), ' ', json_encode($t->allowsNull()), ':';
    ///     foreach ($t->getTypes() as $m) { echo ' ', get_class($m), '(', $m, ')'; }
    ///     echo "\n";
    /// }
    /// ```
    /// ```expect
    /// Deprecated: f(): Implicitly marking parameter $b as nullable is deprecated, the explicit nullable type must be used instead in %s on line 3
    /// (X&Y)|null ReflectionUnionType true: ReflectionIntersectionType(X&Y) ReflectionNamedType(null)
    /// (X&Y)|null ReflectionUnionType true: ReflectionIntersectionType(X&Y) ReflectionNamedType(null)
    /// X&Y ReflectionIntersectionType false: ReflectionNamedType(X) ReflectionNamedType(Y)
    /// ```
    ///
    /// `iterable` in a union was compiled to `Traversable|array`, so those
    /// are its members:
    ///
    /// ```php
    /// class X {}
    /// function f(X|iterable|bool $a) {}
    /// foreach ((new ReflectionFunction('f'))->getParameters()[0]->getType()->getTypes() as $m) { echo $m, ' '; }
    /// ```
    /// ```expect
    /// X Traversable array bool
    /// ```
    fn getTypes(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let reference = this_type(req, this.object())?;
        let parts = parts(&reference.ty);
        let mut members: Vec<Type> = Vec::new();
        for member in &parts.members {
            if matches!(
                member,
                Type::Named(_) | Type::SelfClass | Type::ParentClass | Type::Intersection(_)
            ) {
                members.push(member.clone());
            }
        }
        for wanted in [
            Type::CalledClass,
            Type::Callable,
            Type::Object,
            Type::Array,
            Type::String,
            Type::Int,
            Type::Float,
            Type::Bool,
            Type::True,
            Type::False,
            Type::Iterable,
        ] {
            if parts.members.contains(&wanted) {
                members.push(wanted);
            }
        }
        if parts.nullable {
            members.push(Type::Null);
        }
        let mut types = PhpArray::new(req)?;
        for member in &members {
            let object = type_factory(req, member, false)?;
            types.append(req, object.into_value())?;
        }
        Ok(types)
    }
}

/// `ReflectionIntersectionType` as the stub declares it.
#[php_class(extends(ReflectionType), size = 96, payload = crate::Reflected, handlers = crate::HANDLERS)]
pub(crate) struct ReflectionIntersectionType;

#[php_class]
impl ReflectionIntersectionType {
    #[php(implementation = ReflectionUnionType::getTypes)]
    fn getTypes(this: This<Self>) -> PhpArray;
}
