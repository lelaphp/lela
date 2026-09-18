//! The accessors `php_reflection.c` answers off one `zend_function`:
//! `ReflectionFunctionAbstract`'s, which a `ReflectionFunction` and a
//! `ReflectionMethod` both inherit, and the modifier and scope
//! accessors `ReflectionMethod` adds on top. Each reads the
//! declaration the reflector's `$name`, and a method's `$class`, look
//! up again, through [`super::signature::this_function`].
//!
//! What stands beside `stub::unbuilt` still: `isGenerator` and
//! `getStaticVariables`, whose facts the declaration model has no field
//! for (#420); `getEndLine`, which is #376's; the four closure
//! accessors and `getClosure`, which wait on a closure's declaration
//! reaching an extension (#415); `invoke` and `invokeArgs`, which wait
//! on `api::call::call`; `setAccessible` and `ReflectionFunction`'s
//! `isDisabled`, which the stub marks `#[\Deprecated]` and the engine
//! does not raise the deprecation of yet (#412).

use php_ext::decl::{FunctionDecl, MethodDecl, Visibility};
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, PhpArray, Value};

use super::signature::{FunctionRef, this_function, type_factory};
use super::{
    at_most_arguments, class_name_of, class_named, exactly_arguments, is_constructor,
    linked_methods, namespace_end, new_object, one_string_argument, reflection_class_factory,
    set_name, set_string_prop, string_value, this_name,
};

/// `ZEND_ACC_PUBLIC`, `ZEND_ACC_PROTECTED` and `ZEND_ACC_PRIVATE`, the
/// `ZEND_ACC_PPP_MASK` bits the `IS_*` constants of the stub name.
fn visibility_bit(visibility: Visibility) -> i64 {
    match visibility {
        Visibility::Public => 1,
        Visibility::Protected => 2,
        Visibility::Private => 4,
    }
}

/// `private function __clone(): void {}`: php declares it on every
/// reflector with an empty body, so that a clone is refused by the
/// visibility and never by the body.
///
/// ```php
/// echo new ReflectionMethod('ReflectionClass', '__clone');
/// ```
/// ```expect
/// Method [ <internal:Reflection> private method __clone ] {
///
///   - Parameters [0] {
///   }
///   - Return [ void ]
/// }
/// ```
pub(super) fn reflection_clone(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    Ok(Value::null())
}

/// The method a `ReflectionMethod` reflects, with the class its
/// `$class` names: php's `GET_REFLECTION_OBJECT_PTR(mptr)` beside
/// `intern->ce`.
fn this_method(req: &mut Request) -> Result<(ClassId, MethodDecl, ClassId), Thrown> {
    let (_, name) = this_name(req, "ReflectionMethod")?;
    let Some(class) = req
        .this()
        .and_then(|this| this.prop(req, b"class"))
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Err(throw(
            req,
            "Error",
            "Typed property ReflectionMethod::$class must not be accessed before initialization",
        ));
    };
    let class = class_named(req, &class)?;
    let lowered = name.to_ascii_lowercase();
    let Some(found) = linked_methods(req, class)
        .into_iter()
        .find(|m| m.decl.function.name.0.to_ascii_lowercase() == lowered)
    else {
        let named = class_name_of(req, class);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Method {}::{}() does not exist",
                String::from_utf8_lossy(&named),
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    Ok((class, found.decl, found.scope))
}

/// `ReflectionFunctionAbstract::getName()`:
/// `fptr->common.function_name`.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->getName());
/// var_dump((new ReflectionMethod('Exception', 'getCode'))->getName());
/// ```
/// ```expect
/// string(11) "json_encode"
/// string(7) "getCode"
/// ```
pub(super) fn reflection_function_get_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    let name = decl.name.0.to_vec();
    string_value(req, &name)
}

/// `ReflectionFunctionAbstract::getShortName()`: the name after the
/// last `\`, and the whole name where there is none.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->getShortName());
/// ```
/// ```expect
/// string(11) "json_encode"
/// ```
pub(super) fn reflection_function_get_short_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    let name = decl.name.0.to_vec();
    let start = namespace_end(&name).map_or(0, |end| end + 1);
    let short = name[start..].to_vec();
    string_value(req, &short)
}

/// `ReflectionFunctionAbstract::inNamespace()`: whether the name has a
/// `\` in it.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->inNamespace());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_function_in_namespace(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    Ok(Value::bool(namespace_end(&decl.name.0).is_some()))
}

/// `ReflectionFunctionAbstract::getNamespaceName()`: the name before
/// the last `\`, and the empty string where there is none.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->getNamespaceName());
/// ```
/// ```expect
/// string(0) ""
/// ```
pub(super) fn reflection_function_get_namespace_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    let name = decl.name.0.to_vec();
    let space = namespace_end(&name).map_or_else(Vec::new, |end| name[..end].to_vec());
    string_value(req, &space)
}

/// Whether the function is php's `ZEND_INTERNAL_FUNCTION`: a native,
/// whose declaration names no file, as `_function_string` reads it for
/// `<internal`.
fn is_internal(decl: &FunctionDecl) -> bool {
    decl.file.0.is_empty()
}

/// `ReflectionFunctionAbstract::isInternal()`: whether `fptr->type` is
/// `ZEND_INTERNAL_FUNCTION`.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->isInternal());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_function_is_internal(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    Ok(Value::bool(is_internal(&decl)))
}

/// `ReflectionFunctionAbstract::isUserDefined()`: whether `fptr->type`
/// is `ZEND_USER_FUNCTION`.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->isUserDefined());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_function_is_user_defined(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    Ok(Value::bool(!is_internal(&decl)))
}

/// `ReflectionFunctionAbstract::getFileName()`:
/// `op_array.filename`, and `false` for a native.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->getFileName());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_function_get_file_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    if is_internal(&decl) {
        return Ok(Value::bool(false));
    }
    let file = decl.file.0.to_vec();
    string_value(req, &file)
}

/// `ReflectionFunctionAbstract::getStartLine()`:
/// `op_array.line_start`, and `false` for a native.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->getStartLine());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_function_get_start_line(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    if is_internal(&decl) {
        return Ok(Value::bool(false));
    }
    Ok(Value::int(i64::from(decl.line)))
}

/// `ReflectionFunctionAbstract::getDocComment()`:
/// `op_array.doc_comment`, and `false` where there is none.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->getDocComment());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_function_get_doc_comment(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    let Some(doc) = decl.doc_comment.clone() else {
        return Ok(Value::bool(false));
    };
    string_value(req, &doc)
}

/// `ReflectionFunctionAbstract::returnsReference()`:
/// `ZEND_ACC_RETURN_REFERENCE`.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->returnsReference());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_function_returns_reference(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    Ok(Value::bool(decl.returns_by_ref))
}

/// `ReflectionFunctionAbstract::isVariadic()`: `ZEND_ACC_VARIADIC`,
/// the bit the last parameter's `...` sets.
///
/// ```php
/// var_dump((new ReflectionMethod('ReflectionClass', 'newInstance'))->isVariadic());
/// var_dump((new ReflectionFunction('json_encode'))->isVariadic());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_function_is_variadic(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    Ok(Value::bool(decl.parameters.iter().any(|p| p.variadic)))
}

/// `ReflectionFunctionAbstract::isStatic()`: `ZEND_ACC_STATIC`, which
/// a function outside a class never carries.
///
/// ```php
/// var_dump((new ReflectionMethod('ReflectionMethod', 'createFromMethodName'))->isStatic());
/// var_dump((new ReflectionFunction('json_encode'))->isStatic());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_function_is_static(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (function, _) = this_function(req)?;
    let FunctionRef::Method(class, name) = &function else {
        return Ok(Value::bool(false));
    };
    let (class, name) = (*class, name.clone());
    let decl = method_decl(req, class, &name);
    Ok(Value::bool(decl.is_some_and(|decl| !decl.is_instance)))
}

/// One method of a linked class by its name, `zend_hash_find_ptr` over
/// the function table.
fn method_decl(req: &mut Request, class: ClassId, name: &[u8]) -> Option<MethodDecl> {
    let lowered = name.to_ascii_lowercase();
    linked_methods(req, class)
        .into_iter()
        .find(|m| m.decl.function.name.0.to_ascii_lowercase() == lowered)
        .map(|m| m.decl)
}

/// Whether the declared return type is the tentative one a native
/// method may carry.
fn tentative_return(req: &mut Request, function: &FunctionRef) -> bool {
    let FunctionRef::Method(class, name) = function else {
        return false;
    };
    let (class, name) = (*class, name.clone());
    method_decl(req, class, &name).is_some_and(|decl| decl.tentative_return)
}

/// `ReflectionFunctionAbstract::hasReturnType()`:
/// `ZEND_ACC_HAS_RETURN_TYPE` and not the tentative bit.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->hasReturnType());
/// var_dump((new ReflectionMethod('JsonSerializable', 'jsonSerialize'))->hasReturnType());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_function_has_return_type(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (function, decl) = this_function(req)?;
    let tentative = tentative_return(req, &function);
    Ok(Value::bool(decl.return_type.is_some() && !tentative))
}

/// `ReflectionFunctionAbstract::getReturnType()`:
/// `reflection_type_factory` over `arg_info[-1]`, and `null` where
/// there is no return type or the type is tentative.
///
/// ```php
/// echo (new ReflectionFunction('json_encode'))->getReturnType(), "\n";
/// var_dump((new ReflectionMethod('JsonSerializable', 'jsonSerialize'))->getReturnType());
/// ```
/// ```expect
/// string|false
/// NULL
/// ```
pub(super) fn reflection_function_get_return_type(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (function, decl) = this_function(req)?;
    let tentative = tentative_return(req, &function);
    let Some(ty) = decl.return_type.filter(|_| !tentative) else {
        return Ok(Value::null());
    };
    let object = type_factory(req, &ty, true)?;
    Ok(object.as_value())
}

/// `ReflectionFunctionAbstract::hasTentativeReturnType()`:
/// `ZEND_ACC_HAS_RETURN_TYPE` and the tentative bit.
///
/// ```php
/// var_dump((new ReflectionMethod('JsonSerializable', 'jsonSerialize'))->hasTentativeReturnType());
/// var_dump((new ReflectionFunction('json_encode'))->hasTentativeReturnType());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_function_has_tentative_return_type(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (function, decl) = this_function(req)?;
    let tentative = tentative_return(req, &function);
    Ok(Value::bool(decl.return_type.is_some() && tentative))
}

/// `ReflectionFunctionAbstract::getTentativeReturnType()`:
/// `reflection_type_factory` over `arg_info[-1]` where the tentative
/// bit is set, and `null` otherwise.
///
/// ```php
/// echo (new ReflectionMethod('JsonSerializable', 'jsonSerialize'))->getTentativeReturnType(), "\n";
/// var_dump((new ReflectionFunction('json_encode'))->getTentativeReturnType());
/// ```
/// ```expect
/// mixed
/// NULL
/// ```
pub(super) fn reflection_function_get_tentative_return_type(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (function, decl) = this_function(req)?;
    let tentative = tentative_return(req, &function);
    let Some(ty) = decl.return_type.filter(|_| tentative) else {
        return Ok(Value::null());
    };
    let object = type_factory(req, &ty, true)?;
    Ok(object.as_value())
}

/// `ReflectionFunctionAbstract::getExtensionName()`:
/// `internal->module->name`, and `false` for a user function.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->getExtensionName());
/// var_dump((new ReflectionMethod('Exception', 'getCode'))->getExtensionName());
/// ```
/// ```expect
/// string(4) "json"
/// string(4) "Core"
/// ```
pub(super) fn reflection_function_get_extension_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let Some(name) = function_extension_name(req)? else {
        return Ok(Value::bool(false));
    };
    string_value(req, name.as_bytes())
}

/// `ReflectionFunctionAbstract::getExtension()`:
/// `reflection_extension_factory` over the same name, and `null`
/// where `getExtensionName()` is `false`. Zend's own natives and
/// classes name `Core` and the bundle has no value under it, a
/// question #379 asks, so a method of a Zend class answers `null`
/// where php answers the `Core` reflector.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->getExtension());
/// ```
/// ```expect
/// object(ReflectionExtension)#2 (1) {
///   ["name"]=>
///   string(4) "json"
/// }
/// ```
pub(super) fn reflection_function_get_extension(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let Some(name) = function_extension_name(req)? else {
        return Ok(Value::null());
    };
    if req.extension_named(name.as_bytes()).is_none() {
        return Ok(Value::null());
    }
    let class = class_named(req, b"ReflectionExtension")?;
    let object = new_object(req, class)?;
    set_name(req, object, name.as_bytes())?;
    Ok(object.as_value())
}

/// The extension that declared the function, `internal->module`: for a
/// method it is the class's, `ce->info.internal.module`, as php reads
/// it off the same entry.
fn function_extension_name(req: &mut Request) -> Result<Option<String>, Thrown> {
    let (function, decl) = this_function(req)?;
    if !is_internal(&decl) {
        return Ok(None);
    }
    match function {
        FunctionRef::Method(class, _) => Ok(req.class_extension_name(class).map(str::to_owned)),
        FunctionRef::Global(name) => {
            let found = super::lookup_function(req, &name);
            Ok(found
                .and_then(|found| req.function_extension(found))
                .and_then(|e| req.extension_name(e))
                .map(str::to_owned))
        }
    }
}

/// `ReflectionFunctionAbstract::isClosure()`: `ZEND_ACC_CLOSURE`. No
/// reflector here holds a closure yet (#415), so the answer is php's
/// for every function that is not one.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->isClosure());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_function_is_closure(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    this_function(req)?;
    Ok(Value::bool(false))
}

/// `ReflectionFunction::isAnonymous()`: `ZEND_ACC_CLOSURE` again, on
/// the subclass; the same answer and the same wait (#415).
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->isAnonymous());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_function_is_anonymous(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    this_function(req)?;
    Ok(Value::bool(false))
}

/// `ReflectionFunctionAbstract::getAttributes(?string $name = null,
/// int $flags = 0)`: `reflect_attributes` over
/// `fptr->common.attributes`. `ReflectionAttribute` is declared and
/// not served (#414), so a function that carries one is php's
/// `ReflectionException` for the missing class rather than a wrong
/// list.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->getAttributes());
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
pub(super) fn reflection_function_get_attributes(req: &mut Request) -> Result<Value, Thrown> {
    at_most_arguments(req, 2)?;
    let (_, decl) = this_function(req)?;
    if !decl.attributes.is_empty() {
        return Err(throw(
            req,
            "ReflectionException",
            "Class \"ReflectionAttribute\" does not exist",
        ));
    }
    Ok(Value::array(PhpArray::new(req)?))
}

// ------------------------------------------------- ReflectionMethod

/// `ReflectionMethod::isPublic()`: `ZEND_ACC_PUBLIC`.
///
/// ```php
/// var_dump((new ReflectionMethod('Exception', 'getCode'))->isPublic());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_method_is_public(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl, _) = this_method(req)?;
    Ok(Value::bool(decl.visibility == Visibility::Public))
}

/// `ReflectionMethod::isProtected()`: `ZEND_ACC_PROTECTED`.
///
/// ```php
/// var_dump((new ReflectionMethod('Exception', 'getCode'))->isProtected());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_method_is_protected(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl, _) = this_method(req)?;
    Ok(Value::bool(decl.visibility == Visibility::Protected))
}

/// `ReflectionMethod::isPrivate()`: `ZEND_ACC_PRIVATE`.
///
/// ```php
/// var_dump((new ReflectionMethod('Exception', '__clone'))->isPrivate());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_method_is_private(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl, _) = this_method(req)?;
    Ok(Value::bool(decl.visibility == Visibility::Private))
}

/// `ReflectionMethod::isAbstract()`: `ZEND_ACC_ABSTRACT`.
///
/// ```php
/// var_dump((new ReflectionMethod('JsonSerializable', 'jsonSerialize'))->isAbstract());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_method_is_abstract(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl, _) = this_method(req)?;
    Ok(Value::bool(decl.is_abstract))
}

/// `ReflectionMethod::isFinal()`: `ZEND_ACC_FINAL`.
///
/// ```php
/// var_dump((new ReflectionMethod('Exception', 'getCode'))->isFinal());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_method_is_final(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl, _) = this_method(req)?;
    Ok(Value::bool(decl.is_final))
}

/// `ReflectionMethod::isConstructor()`: `ZEND_ACC_CTOR`, and the
/// class's constructor being this method's own, so an inherited
/// constructor read off the child is still the constructor.
///
/// ```php
/// var_dump((new ReflectionMethod('Exception', '__construct'))->isConstructor());
/// var_dump((new ReflectionMethod('Exception', 'getCode'))->isConstructor());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_method_is_constructor(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl, _) = this_method(req)?;
    Ok(Value::bool(is_constructor(&decl)))
}

/// `ReflectionMethod::isDestructor()`: the method named `__destruct`.
///
/// ```php
/// var_dump((new ReflectionMethod('Exception', '__construct'))->isDestructor());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_method_is_destructor(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl, _) = this_method(req)?;
    Ok(Value::bool(
        decl.function.name.0.eq_ignore_ascii_case(b"__destruct"),
    ))
}

/// `ReflectionMethod::getModifiers()`: the `ZEND_ACC_PPP_MASK`,
/// `ZEND_ACC_STATIC`, `ZEND_ACC_ABSTRACT` and `ZEND_ACC_FINAL` bits
/// of `fn_flags`, the numbers the stub's `IS_*` constants name.
///
/// ```php
/// var_dump((new ReflectionMethod('Exception', 'getCode'))->getModifiers());
/// var_dump((new ReflectionMethod('JsonSerializable', 'jsonSerialize'))->getModifiers());
/// ```
/// ```expect
/// int(33)
/// int(65)
/// ```
pub(super) fn reflection_method_get_modifiers(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl, _) = this_method(req)?;
    let mut bits = visibility_bit(decl.visibility);
    if !decl.is_instance {
        bits |= 16;
    }
    if decl.is_final {
        bits |= 32;
    }
    if decl.is_abstract {
        bits |= 64;
    }
    Ok(Value::int(bits))
}

/// `ReflectionMethod::getDeclaringClass()`:
/// `zend_reflection_class_factory` over `mptr->common.scope`.
///
/// ```php
/// var_dump((new ReflectionMethod('JsonException', 'getCode'))->getDeclaringClass());
/// ```
/// ```expect
/// object(ReflectionClass)#2 (1) {
///   ["name"]=>
///   string(9) "Exception"
/// }
/// ```
pub(super) fn reflection_method_get_declaring_class(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, scope) = this_method(req)?;
    let object = reflection_class_factory(req, scope)?;
    Ok(object.as_value())
}

/// `ReflectionMethod::hasPrototype()`: whether
/// `mptr->common.prototype` is set, the interface or abstract parent
/// method `do_inheritance_check_on_method` gave it.
///
/// ```php
/// var_dump((new ReflectionMethod('JsonException', 'getCode'))->hasPrototype());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_method_has_prototype(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, name) = this_name(req, "ReflectionMethod")?;
    Ok(Value::bool(prototype_of(req, &name)?.is_some()))
}

/// `mptr->common.prototype`'s scope for the method `$name` names on
/// the reflector's class.
fn prototype_of(req: &mut Request, name: &[u8]) -> Result<Option<ClassId>, Thrown> {
    let Some(class) = req
        .this()
        .and_then(|this| this.prop(req, b"class"))
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Ok(None);
    };
    let class = class_named(req, &class)?;
    let lowered = name.to_ascii_lowercase();
    Ok(linked_methods(req, class)
        .into_iter()
        .find(|m| m.decl.function.name.0.to_ascii_lowercase() == lowered)
        .and_then(|m| m.prototype))
}

/// `ReflectionMethod::getPrototype()`: a `ReflectionMethod` over the
/// prototype with its own scope, and php's `ReflectionException` for a
/// method that has none.
///
/// ```php
/// var_dump((new ReflectionMethod('JsonException', 'getCode'))->getPrototype()->getName());
/// var_dump((new ReflectionMethod('JsonException', 'getCode'))->getPrototype()->getDeclaringClass()->getName());
/// ```
/// ```expect
/// string(7) "getCode"
/// string(9) "Throwable"
/// ```
///
/// ```php
/// (new ReflectionMethod('Exception', '__construct'))->getPrototype();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Method Exception::__construct does not have a prototype in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionMethod->getPrototype()
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(super) fn reflection_method_get_prototype(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, name) = this_name(req, "ReflectionMethod")?;
    let Some(prototype) = prototype_of(req, &name)? else {
        let (class, _, _) = this_method(req)?;
        let class = class_name_of(req, class);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Method {}::{} does not have a prototype",
                String::from_utf8_lossy(&class),
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    let class = class_named(req, b"ReflectionMethod")?;
    let object = new_object(req, class)?;
    let scope = class_name_of(req, prototype);
    set_name(req, object, &name)?;
    set_string_prop(req, object, b"class", &scope)?;
    Ok(object.as_value())
}

/// `ReflectionMethod::createFromMethodName(string $method)`: the
/// factory php's 8.4 deprecation of the one-argument constructor
/// points at, over `Class::method`.
///
/// ```php
/// var_dump(ReflectionMethod::createFromMethodName('Exception::getCode')->getName());
/// var_dump(ReflectionMethod::createFromMethodName('Exception::getCode')->getDeclaringClass()->getName());
/// ```
/// ```expect
/// string(7) "getCode"
/// string(9) "Exception"
/// ```
///
/// ```php
/// ReflectionMethod::createFromMethodName('nocolons');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: ReflectionMethod::createFromMethodName(): Argument #1 ($method) must be a valid method name in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionMethod::createFromMethodName('nocolons')
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(super) fn reflection_method_create_from_method_name(
    req: &mut Request,
) -> Result<Value, Thrown> {
    let given = one_string_argument(req)?;
    let Some(at) = given.windows(2).position(|pair| pair == b"::") else {
        return Err(throw(
            req,
            "ReflectionException",
            "ReflectionMethod::createFromMethodName(): Argument #1 ($method) must be a valid method name",
        ));
    };
    let class = class_named(req, &given[..at])?;
    let name = given[at + 2..].to_vec();
    let lowered = name.to_ascii_lowercase();
    let Some(found) = linked_methods(req, class)
        .into_iter()
        .find(|m| m.decl.function.name.0.to_ascii_lowercase() == lowered)
    else {
        let named = class_name_of(req, class);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Method {}::{}() does not exist",
                String::from_utf8_lossy(&named),
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    let reflector = class_named(req, b"ReflectionMethod")?;
    let object = new_object(req, reflector)?;
    let scope = class_name_of(req, found.scope);
    set_name(req, object, &found.decl.function.name.0)?;
    set_string_prop(req, object, b"class", &scope)?;
    Ok(object.as_value())
}
