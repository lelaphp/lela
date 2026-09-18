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
//!
//! `getClass`, `isArray` and `isCallable` stand beside `stub::unbuilt`
//! still: the stub marks all three `#[\Deprecated]` and php prints the
//! deprecation on every call, which the engine does not raise yet
//! (#412), so a witness beside a body here would pin a line the engine
//! never prints.

use php_ext::call::eval_const_expr;
use php_ext::decl::{ConstExpr, FunctionDecl, Parameter, Type};
use php_ext::raising::{Thrown, arg_type_error, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, Value, ValueRef};

use super::{
    State, at_most_arguments, class_name_of, class_named, exactly_arguments, linked_methods,
    lookup_function, new_object, parameter_string, reflection_class_factory, required_parameters,
    set_name, set_string_prop, string_value, this_name, type_to_string,
};

/// The function a `ReflectionParameter` reflects, `param->fptr` as a
/// name this engine can look the declaration up with again: a global
/// function, a method with the class that declared it, or a closure,
/// whose `{closure:…}` name no table holds, so what stands for it is
/// the object itself, as php's `intern->obj` does.
#[derive(Clone)]
pub(super) enum FunctionRef {
    Global(Vec<u8>),
    Method(ClassId, Vec<u8>),
    Closure(PhpObject),
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
            members: members.clone(),
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

/// `get_type_kind`: a list is its list's kind; `iterable` and `mixed`
/// are named for backwards compatibility however wide their mask is;
/// anything else with more than one member without `null` is a union.
fn kind_of(ty: &Type) -> Kind {
    let parts = parts(ty);
    if parts.intersection {
        return Kind::Intersection;
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
) -> Result<PhpObject, Thrown> {
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
    req.state::<State>()
        .types
        .insert(object.index(), TypeRef { ty, legacy });
    Ok(object)
}

/// `GET_REFLECTION_OBJECT_PTR` for a type reflector.
fn this_type(req: &mut Request) -> Result<TypeRef, Thrown> {
    let found = req
        .this()
        .and_then(|this| req.state::<State>().types.get(&this.index()).cloned());
    match found {
        Some(reference) => Ok(reference),
        None => Err(throw(
            req,
            "Error",
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
        _ => type_text(&Type::Union(parts.members)),
    }
}

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
pub(super) fn reflection_type_allows_null(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let reference = this_type(req)?;
    let allows = matches!(reference.ty, Type::Mixed) || parts(&reference.ty).nullable;
    Ok(Value::bool(allows))
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
pub(super) fn reflection_type_to_string(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let reference = this_type(req)?;
    let text = type_text(&reference.ty);
    string_value(req, text.as_bytes())
}

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
pub(super) fn reflection_named_type_get_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let reference = this_type(req)?;
    let text = if reference.legacy {
        type_text_without_null(&reference.ty)
    } else {
        type_text(&reference.ty)
    };
    string_value(req, text.as_bytes())
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
pub(super) fn reflection_named_type_is_builtin(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let reference = this_type(req)?;
    let named = parts(&reference.ty).members.iter().any(|member| {
        matches!(
            member,
            Type::Named(_) | Type::SelfClass | Type::ParentClass | Type::CalledClass
        )
    });
    Ok(Value::bool(!named))
}

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
pub(super) fn reflection_union_type_get_types(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let reference = this_type(req)?;
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
        types.append(req, object.as_value())?;
    }
    Ok(Value::array(types))
}

// ------------------------------------------------ ReflectionParameter

/// The declaration of the function a `ParameterRef` names, looked up
/// again as every reflector here looks its subject up again.
fn function_decl_of(req: &mut Request, function: &FunctionRef) -> Option<FunctionDecl> {
    match function {
        FunctionRef::Closure(object) => closure_decl(req, *object),
        FunctionRef::Global(name) => {
            let found = lookup_function(req, name)?;
            req.function_decl(found).cloned()
        }
        FunctionRef::Method(class, name) => {
            let lowered = name.to_ascii_lowercase();
            linked_methods(req, *class)
                .into_iter()
                .find(|m| m.decl.function.name.0.to_ascii_lowercase() == lowered)
                .map(|m| m.decl.function)
        }
    }
}

/// `param->fptr->common.scope`.
fn scope_of(req: &Request, function: &FunctionRef) -> Option<ClassId> {
    match function {
        FunctionRef::Global(_) => None,
        FunctionRef::Method(class, _) => Some(*class),
        // A closure declared in a method carries that class as its
        // scope, and `zend_get_closure_method_def` hands the
        // declaration back whatever the scope is, so the scope a
        // parameter's `declaringClass` reads is the closure's own.
        FunctionRef::Closure(object) => object.closure(req)?.scope,
    }
}

/// `zend_get_closure_method_def`: the declaration behind a `Closure`,
/// which is the op array the `DECLARE_LAMBDA_FUNCTION` that made it
/// named; none for an object that is no closure.
pub(super) fn closure_decl(req: &mut Request, object: PhpObject) -> Option<FunctionDecl> {
    let function = object.closure(req)?.function;
    req.function_decl(function).cloned()
}

/// One `ReflectionParameter` over `offset` of `function`, php's
/// `reflection_parameter_factory`: `$name` written from the declared
/// parameter and the reference kept where php keeps `intern->ptr`.
pub(super) fn parameter_factory(
    req: &mut Request,
    function: &FunctionRef,
    decl: &FunctionDecl,
    offset: usize,
) -> Result<PhpObject, Thrown> {
    let class = class_named(req, b"ReflectionParameter")?;
    let object = new_object(req, class)?;
    let name = decl
        .parameters
        .get(offset)
        .map(|p| p.name.0.to_vec())
        .unwrap_or_default();
    set_name(req, object, &name)?;
    let reference = ParameterRef {
        function: function.clone(),
        offset,
        required: offset < required_parameters(decl),
    };
    req.state::<State>()
        .parameters
        .insert(object.index(), reference);
    Ok(object)
}

/// `GET_REFLECTION_OBJECT_PTR` for a parameter reflector, with the
/// declaration and the one parameter it points at.
fn this_parameter(req: &mut Request) -> Result<(ParameterRef, FunctionDecl, Parameter), Thrown> {
    let found = req
        .this()
        .and_then(|this| req.state::<State>().parameters.get(&this.index()).cloned());
    let Some(reference) = found else {
        return Err(throw(
            req,
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        ));
    };
    let Some(decl) = function_decl_of(req, &reference.function) else {
        return Err(throw(
            req,
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        ));
    };
    let Some(parameter) = decl.parameters.get(reference.offset).cloned() else {
        return Err(throw(
            req,
            "Error",
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
/// element is an object, and an object is a closure, which no seam
/// hands an extension yet (#415).
fn construct_function(req: &mut Request) -> Result<FunctionRef, Thrown> {
    let first = req.argument(1).unwrap_or_default().deref();
    let function = match first.as_ref() {
        ValueRef::Array(array) => {
            let class = array.get(&Key::Int(0));
            let method = array.get(&Key::Int(1));
            let (Some(class), Some(method)) = (class, method) else {
                return Err(throw(
                    req,
                    "ReflectionException",
                    "Expected array($object, $method) or array($classname, $method)",
                ));
            };
            let class = if let ValueRef::Object(object) = class.deref().as_ref() {
                object.class()
            } else {
                let name = class.as_str_bytes().unwrap_or_default().to_vec();
                class_named(req, &name)?
            };
            let method = method.as_str_bytes().unwrap_or_default().to_vec();
            let lowered = method.to_ascii_lowercase();
            let Some(found) = linked_methods(req, class)
                .into_iter()
                .find(|m| m.decl.function.name.0.to_ascii_lowercase() == lowered)
            else {
                let class = class_name_of(req, class);
                return Err(throw(
                    req,
                    "ReflectionException",
                    &format!(
                        "Method {}::{}() does not exist",
                        String::from_utf8_lossy(&class),
                        String::from_utf8_lossy(&method)
                    ),
                ));
            };
            FunctionRef::Method(found.scope, found.decl.function.name.0.to_vec())
        }
        // php's `IS_OBJECT` arm: `zend_get_closure_method_def` where
        // the object is a `Closure`, and the argument error for any
        // other object, since no class here declares `__invoke` that
        // php would reach through `zend_std_get_closure`.
        ValueRef::Object(object) => {
            let object = *object;
            if closure_decl(req, object).is_none() {
                return Err(arg_type_error(
                    req,
                    1,
                    &format!(
                        "be a string, an array(class, method), or a callable object, {} given",
                        first.type_name()
                    ),
                ));
            }
            FunctionRef::Closure(object)
        }
        _ => {
            let name = super::string_argument(req, 1)?;
            let bare = name.strip_prefix(b"\\").unwrap_or(&name);
            let Some(found) = lookup_function(req, bare) else {
                return Err(throw(
                    req,
                    "ReflectionException",
                    &format!(
                        "Function {}() does not exist",
                        String::from_utf8_lossy(&name)
                    ),
                ));
            };
            let declared = req
                .function_decl(found)
                .map(|decl| decl.name.0.to_vec())
                .unwrap_or_default();
            FunctionRef::Global(declared)
        }
    };
    Ok(function)
}

/// The parameter argument of `ReflectionParameter::__construct`,
/// php's `Z_PARAM_STR_OR_LONG`: the offset of the parameter of that
/// name, or the offset itself, held to the declared count.
fn construct_offset(req: &mut Request, decl: &FunctionDecl) -> Result<usize, Thrown> {
    let second = req.argument(2).unwrap_or_default().deref();
    let offset = if matches!(second.as_ref(), ValueRef::Str(_)) {
        let wanted = second.as_str_bytes().unwrap_or_default().to_vec();
        let found = decl
            .parameters
            .iter()
            .position(|p| p.name.0.as_ref() == wanted.as_slice());
        match found {
            Some(offset) => offset,
            None => {
                return Err(throw(
                    req,
                    "ReflectionException",
                    "The parameter specified by its name could not be found",
                ));
            }
        }
    } else {
        {
            let position = php_ext::coerce::to_long(req, &second)?;
            if position < 0 {
                return Err(throw(
                    req,
                    "ValueError",
                    "ReflectionParameter::__construct(): Argument #2 ($param) must be greater than or equal to 0",
                ));
            }
            let position = usize::try_from(position).unwrap_or(usize::MAX);
            if position >= decl.parameters.len() {
                return Err(throw(
                    req,
                    "ReflectionException",
                    "The parameter specified by its offset could not be found",
                ));
            }
            position
        }
    };
    Ok(offset)
}

pub(super) fn reflection_parameter_construct(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 2)?;
    let function = construct_function(req)?;
    let Some(decl) = function_decl_of(req, &function) else {
        return Err(throw(
            req,
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        ));
    };
    let offset = construct_offset(req, &decl)?;
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
    if let Some(this) = req.this() {
        set_name(req, this, &name)?;
        // `intern->obj`: a parameter of a closure keeps the closure, as
        // php's does, so the row's `FunctionRef::Closure` names an
        // object that stands for as long as the reflector does.
        if let FunctionRef::Closure(closure) = reference.function {
            super::keep_closure(req, this, closure);
        }
        req.state::<State>()
            .parameters
            .insert(this.index(), reference);
    }
    Ok(Value::null())
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
pub(super) fn reflection_parameter_to_string(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (reference, decl, parameter) = this_parameter(req)?;
    let internal = decl.file.0.is_empty();
    let text = parameter_string(
        req,
        &parameter,
        reference.offset,
        reference.required,
        internal,
    );
    string_value(req, text.as_bytes())
}

/// `ReflectionParameter::getName()`: `arg_info->name`.
///
/// ```php
/// var_dump((new ReflectionParameter('str_repeat', 0))->getName());
/// ```
/// ```expect
/// string(6) "string"
/// ```
pub(super) fn reflection_parameter_get_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, parameter) = this_parameter(req)?;
    let name = parameter.name.0.to_vec();
    string_value(req, &name)
}

/// `ReflectionParameter::getPosition()`: `param->offset`.
///
/// ```php
/// var_dump((new ReflectionParameter('str_repeat', 1))->getPosition());
/// ```
/// ```expect
/// int(1)
/// ```
pub(super) fn reflection_parameter_get_position(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (reference, _, _) = this_parameter(req)?;
    Ok(Value::int(i64::try_from(reference.offset).unwrap_or(0)))
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
pub(super) fn reflection_parameter_is_optional(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (reference, _, _) = this_parameter(req)?;
    Ok(Value::bool(!reference.required))
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
pub(super) fn reflection_parameter_is_variadic(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, parameter) = this_parameter(req)?;
    Ok(Value::bool(parameter.variadic))
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
pub(super) fn reflection_parameter_is_passed_by_reference(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, parameter) = this_parameter(req)?;
    Ok(Value::bool(parameter.by_ref))
}

/// `ReflectionParameter::canBePassedByValue()`: the way the parameter
/// is sent is not `ZEND_SEND_BY_REF`.
///
/// ```php
/// var_dump((new ReflectionParameter('str_repeat', 0))->canBePassedByValue());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_parameter_can_be_passed_by_value(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, parameter) = this_parameter(req)?;
    Ok(Value::bool(!parameter.by_ref))
}

/// `ReflectionParameter::isPromoted()`:
/// `ZEND_ARG_IS_PROMOTED`, the bit constructor promotion sets. The
/// declaration model carries no such bit (#413), so the answer is
/// php's for every parameter that was not promoted.
///
/// ```php
/// var_dump((new ReflectionParameter('str_repeat', 0))->isPromoted());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_parameter_is_promoted(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    this_parameter(req)?;
    Ok(Value::bool(false))
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
pub(super) fn reflection_parameter_has_type(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, parameter) = this_parameter(req)?;
    Ok(Value::bool(parameter.ty.is_some()))
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
pub(super) fn reflection_parameter_get_type(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, parameter) = this_parameter(req)?;
    let Some(ty) = parameter.ty else {
        return Ok(Value::null());
    };
    let object = type_factory(req, &ty, true)?;
    Ok(object.as_value())
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
pub(super) fn reflection_parameter_allows_null(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, parameter) = this_parameter(req)?;
    let allows = match &parameter.ty {
        None | Some(Type::Mixed) => true,
        Some(ty) => parts(ty).nullable,
    };
    Ok(Value::bool(allows))
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
pub(super) fn reflection_parameter_get_declaring_function(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (reference, decl, _) = this_parameter(req)?;
    let object = match &reference.function {
        FunctionRef::Global(_) => {
            let class = class_named(req, b"ReflectionFunction")?;
            let object = new_object(req, class)?;
            set_name(req, object, &decl.name.0)?;
            object
        }
        // php's `reflection_function_factory` over the closure itself,
        // which is `intern->obj` again, so the reflector it hands back
        // reflects the closure and not a name no table holds.
        FunctionRef::Closure(closure) => {
            let closure = *closure;
            let class = class_named(req, b"ReflectionFunction")?;
            let object = new_object(req, class)?;
            set_name(req, object, &decl.name.0)?;
            super::keep_closure(req, object, closure);
            object
        }
        FunctionRef::Method(scope, name) => {
            let class = class_named(req, b"ReflectionMethod")?;
            let object = new_object(req, class)?;
            let scope = class_name_of(req, *scope);
            set_name(req, object, name)?;
            set_string_prop(req, object, b"class", &scope)?;
            object
        }
    };
    Ok(object.as_value())
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
pub(super) fn reflection_parameter_get_declaring_class(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (reference, _, _) = this_parameter(req)?;
    let Some(scope) = scope_of(req, &reference.function) else {
        return Ok(Value::null());
    };
    let object = reflection_class_factory(req, scope)?;
    Ok(object.as_value())
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
pub(super) fn reflection_parameter_is_default_value_available(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, parameter) = this_parameter(req)?;
    Ok(Value::bool(parameter_default(&parameter).is_some()))
}

/// The default expression php's `get_parameter_default` would hand
/// back: the declared one, and none for the stub's `UNKNOWN`, which
/// is an optional parameter with nothing to show.
fn parameter_default(parameter: &Parameter) -> Option<&ConstExpr> {
    parameter
        .default
        .as_ref()
        .filter(|expr| !super::stub::is_unknown_default(expr))
}

/// php's `reflection_exception` for a parameter whose default
/// `get_parameter_default` could not read.
fn no_default(req: &mut Request) -> Thrown {
    throw(
        req,
        "ReflectionException",
        "Internal error: Failed to retrieve the default value",
    )
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
pub(super) fn reflection_parameter_get_default_value(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (reference, _, parameter) = this_parameter(req)?;
    let Some(expr) = parameter_default(&parameter).cloned() else {
        return Err(no_default(req));
    };
    let scope = scope_of(req, &reference.function);
    eval_const_expr(req, &expr, scope)
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
pub(super) fn reflection_parameter_is_default_value_constant(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, parameter) = this_parameter(req)?;
    let Some(expr) = parameter_default(&parameter) else {
        return Err(no_default(req));
    };
    Ok(Value::bool(matches!(
        expr,
        ConstExpr::Constant(_) | ConstExpr::ClassConstant { .. }
    )))
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
pub(super) fn reflection_parameter_get_default_value_constant_name(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, parameter) = this_parameter(req)?;
    let Some(expr) = parameter_default(&parameter).cloned() else {
        return Err(no_default(req));
    };
    match expr {
        ConstExpr::Constant(name) => {
            let name = name.0.to_vec();
            string_value(req, &name)
        }
        ConstExpr::ClassConstant { class, name } => {
            let text = format!(
                "{}::{}",
                String::from_utf8_lossy(&class.0),
                String::from_utf8_lossy(&name.0)
            );
            string_value(req, text.as_bytes())
        }
        _ => Ok(Value::null()),
    }
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
pub(super) fn reflection_parameter_get_attributes(req: &mut Request) -> Result<Value, Thrown> {
    at_most_arguments(req, 2)?;
    let (_, _, parameter) = this_parameter(req)?;
    if !parameter.attributes.is_empty() {
        return Err(throw(
            req,
            "ReflectionException",
            "Class \"ReflectionAttribute\" does not exist",
        ));
    }
    Ok(Value::array(PhpArray::new(req)?))
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
pub(super) fn this_function(req: &mut Request) -> Result<(FunctionRef, FunctionDecl), Thrown> {
    let (this, name) = this_name(req, "ReflectionFunctionAbstract")?;
    // `intern->obj`, which a reflector over a closure keeps: php reads
    // the declaration back off it with `zend_get_closure_method_def`,
    // since the `{closure:…}` name is in no table to look up.
    if let Some(closure) = super::kept_closure(req, this)
        && let Some(decl) = closure_decl(req, closure)
    {
        return Ok((FunctionRef::Closure(closure), decl));
    }
    let scope = this
        .prop(req, b"class")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
    let function = if let Some(scope) = scope {
        {
            let class = class_named(req, &scope)?;
            let lowered = name.to_ascii_lowercase();
            let Some(found) = linked_methods(req, class)
                .into_iter()
                .find(|m| m.decl.function.name.0.to_ascii_lowercase() == lowered)
            else {
                let class = class_name_of(req, class);
                return Err(throw(
                    req,
                    "ReflectionException",
                    &format!(
                        "Method {}::{}() does not exist",
                        String::from_utf8_lossy(&class),
                        String::from_utf8_lossy(&name)
                    ),
                ));
            };
            FunctionRef::Method(found.scope, found.decl.function.name.0.to_vec())
        }
    } else {
        {
            if lookup_function(req, &name).is_none() {
                return Err(throw(
                    req,
                    "ReflectionException",
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
        return Err(throw(
            req,
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        ));
    };
    Ok((function, decl))
}

/// `ReflectionFunctionAbstract::getParameters()`: one
/// `ReflectionParameter` per declared parameter, in declaration order,
/// the variadic one included.
///
/// ```php
/// var_dump((new ReflectionFunction("json_encode"))->getParameters());
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   object(ReflectionParameter)#2 (1) {
///     ["name"]=>
///     string(5) "value"
///   }
///   [1]=>
///   object(ReflectionParameter)#3 (1) {
///     ["name"]=>
///     string(5) "flags"
///   }
///   [2]=>
///   object(ReflectionParameter)#4 (1) {
///     ["name"]=>
///     string(5) "depth"
///   }
/// }
/// ```
pub(super) fn reflection_function_get_parameters(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (function, decl) = this_function(req)?;
    let mut parameters = PhpArray::new(req)?;
    for offset in 0..decl.parameters.len() {
        let object = parameter_factory(req, &function, &decl, offset)?;
        parameters.append(req, object.as_value())?;
    }
    Ok(Value::array(parameters))
}

/// `ReflectionFunctionAbstract::getNumberOfParameters()`:
/// `fptr->common.num_args`, the variadic parameter counted.
///
/// ```php
/// var_dump((new ReflectionFunction("json_encode"))->getNumberOfParameters());
/// ```
/// ```expect
/// int(3)
/// ```
pub(super) fn reflection_function_get_number_of_parameters(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    Ok(Value::int(
        i64::try_from(decl.parameters.len()).unwrap_or(0),
    ))
}

/// `ReflectionFunctionAbstract::getNumberOfRequiredParameters()`:
/// `fptr->common.required_num_args`.
///
/// ```php
/// var_dump((new ReflectionFunction("json_encode"))->getNumberOfRequiredParameters());
/// ```
/// ```expect
/// int(1)
/// ```
pub(super) fn reflection_function_get_number_of_required_parameters(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    Ok(Value::int(
        i64::try_from(required_parameters(&decl)).unwrap_or(0),
    ))
}
