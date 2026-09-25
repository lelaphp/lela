//! The accessors `php_reflection.c` answers off one `zend_function`:
//! `ReflectionFunctionAbstract`'s, which a `ReflectionFunction` and a
//! `ReflectionMethod` both inherit, and the modifier and scope
//! accessors `ReflectionMethod` adds on top. Each reads the
//! declaration the reflector's `$name`, and a method's `$class`, look
//! up again, through [`super::signature::this_function`].
//!
//! What stands beside `stub::unbuilt` still: `getStaticVariables`,
//! whose table the declaration model has no field for (#420);
//! `setAccessible`
//! and `ReflectionFunction`'s `isDisabled`, which the stub marks
//! `#[\Deprecated]` and the engine does not raise the deprecation of
//! yet (#412).

use php_ext::abi::Callable;
use php_ext::call::{call_array, call_named, closure_from_callable, known_method};
use php_ext::coerce::{is_callable, to_callable};
use php_ext::decl::{FunctionDecl, MethodDecl, Visibility};
use php_ext::raising::{Thrown, arg_count_error, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, Value};

use super::signature::{FunctionRef, this_function, type_factory};
use super::{
    class_name_of, class_named, exactly_arguments, is_constructor, kept_closure, linked_methods,
    method_or_invoke, namespace_end, new_object, one_string_argument, reflection_class_factory,
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
    let kept = req.this().and_then(|this| kept_closure(req, this));
    let Some(found) = method_or_invoke(req, class, &name, kept) else {
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

/// `ReflectionFunction::getClosure()`: the closure a reflector over one
/// keeps, one more owner, since a closure is immutable; else
/// `zend_create_fake_closure` over the function with no scope.
///
/// ```php
/// $c = function () { return 1; };
/// var_dump((new ReflectionFunction($c))->getClosure() === $c, (new ReflectionFunction('strlen'))->getClosure()('abc'));
/// ```
/// ```expect
/// bool(true)
/// int(3)
/// ```
pub(super) fn reflection_function_get_closure(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (function, decl) = this_function(req)?;
    if let FunctionRef::Closure(closure) = function {
        req.addref(&closure.as_value());
        return Ok(closure.as_value());
    }
    let name = Value::string(php_ext::values::PhpString::new(req, &decl.name.0)?);
    let callable = to_callable(req, &name)?;
    Ok(closure_from_callable(req, &callable)?.as_value())
}

/// `ReflectionMethod::getClosure(?object $object = null)`:
/// `zend_create_fake_closure` over the method, a `static` one on its
/// class with no receiver, an instance one on the object given, which
/// must be of the method's class; a missing object is php's
/// `ValueError`, and a closure given for its own `__invoke` is handed
/// back.
///
/// ```php
/// class A { private $v = 'a'; function get() { return $this->v; } static function s() { return static::class; } }
/// class B extends A {}
/// var_dump((new ReflectionMethod('A', 'get'))->getClosure(new B)(), (new ReflectionMethod('B', 's'))->getClosure()());
/// try { (new ReflectionMethod('A', 'get'))->getClosure(); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { (new ReflectionMethod('A', 'get'))->getClosure(new stdClass); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// $c = function () { return 'c'; }; var_dump((new ReflectionMethod($c, '__invoke'))->getClosure($c) === $c);
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "A"
/// ReflectionMethod::getClosure(): Argument #1 ($object) cannot be null for non-static methods
/// Given object is not an instance of the class this method was declared in
/// bool(true)
/// ```
pub(super) fn reflection_method_get_closure(req: &mut Request) -> Result<Value, Thrown> {
    super::at_most_arguments(req, 1)?;
    let given = req.argument(1).unwrap_or_default();
    let ty = php_ext::decl::Type::Nullable(Box::new(php_ext::decl::Type::Object));
    let given = php_ext::coerce::coerce_param(req, &given, &ty, 1)?;
    let (_, decl, scope) = this_method(req)?;
    let name = decl.function.name.clone();
    if !decl.is_instance {
        let Some(callable) = known_method(req, scope, &name, None) else {
            return Ok(Value::null());
        };
        return Ok(closure_from_callable(req, &callable)?.as_value());
    }
    let Some(object) = given.as_object().copied() else {
        return Err(throw(
            req,
            "ValueError",
            "ReflectionMethod::getClosure(): Argument #1 ($object) cannot be null for non-static methods",
        ));
    };
    if !object.instance_of(req, scope) {
        return Err(throw(
            req,
            "ReflectionException",
            "Given object is not an instance of the class this method was declared in",
        ));
    }
    // "This is an original closure object and __invoke is to be called":
    // the closure itself, one more owner.
    if super::is_closure_invoke(req, scope, &name.0.to_ascii_lowercase()) {
        req.addref(&object.as_value());
        return Ok(object.as_value());
    }
    let Some(callable) = known_method(req, scope, &name, Some(object)) else {
        return Ok(Value::null());
    };
    Ok(closure_from_callable(req, &callable)?.as_value())
}

/// `ReflectionFunctionAbstract::getClosureCalledClass()`: for a
/// reflector over a closure, the closure's called scope, or its scope
/// where it has none, as a `ReflectionClass`; nothing for a closure
/// with neither and for a reflector over a named function.
///
/// ```php
/// class A { function make() { return function () {}; } static function s() {} }
/// class B extends A {}
/// $r = fn ($c) => (new ReflectionFunction($c))->getClosureCalledClass();
/// var_dump($r((new B)->make())->name, $r(Closure::fromCallable(['B', 's']))->name, $r(function () {}), (new ReflectionFunction('strlen'))->getClosureCalledClass());
/// ```
/// ```expect
/// string(1) "B"
/// string(1) "B"
/// NULL
/// NULL
/// ```
pub(super) fn reflection_function_get_closure_called_class(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (function, _) = this_function(req)?;
    let FunctionRef::Closure(closure) = function else {
        return Ok(Value::null());
    };
    let class = closure
        .closure(req)
        .and_then(|info| info.called_scope.or(info.scope));
    match class {
        Some(class) => Ok(reflection_class_factory(req, class)?.as_value()),
        None => Ok(Value::null()),
    }
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

/// `ReflectionFunctionAbstract::getEndLine()`: `op_array.line_end`,
/// the line of the closing brace, and `false` for a native.
///
/// ```php
/// function f() {
///     return 1;
/// }
/// class C { function m() {
/// } }
/// $c = function () {
/// };
/// var_dump((new ReflectionFunction('f'))->getEndLine(), (new ReflectionMethod('C', 'm'))->getEndLine(), (new ReflectionFunction($c))->getEndLine(), (new ReflectionFunction('strlen'))->getEndLine());
/// ```
/// ```expect
/// int(4)
/// int(6)
/// int(8)
/// bool(false)
/// ```
pub(super) fn reflection_function_get_end_line(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    if is_internal(&decl) {
        return Ok(Value::bool(false));
    }
    Ok(Value::int(i64::from(decl.end_line)))
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

/// `ReflectionFunctionAbstract::isGenerator()`: `ZEND_ACC_GENERATOR`,
/// which a `yield` of the body's own sets, and which no native carries.
///
/// ```php
/// function g() { yield; }
/// function f() { return function () { yield; }; }
/// class C { function m() { yield 1; } }
/// var_dump((new ReflectionFunction('g'))->isGenerator(), (new ReflectionFunction('f'))->isGenerator());
/// var_dump((new ReflectionMethod('C', 'm'))->isGenerator(), (new ReflectionFunction('strlen'))->isGenerator());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_function_is_generator(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, decl) = this_function(req)?;
    Ok(Value::bool(decl.is_generator))
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
        // A closure is a user function, which `is_internal` answered
        // for above, so nothing reaches here.
        // A closure's `__invoke` is made with a null `module`.
        FunctionRef::Closure(_) | FunctionRef::Invoke(_) => Ok(None),
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

/// `ReflectionFunctionAbstract::isClosure()`: `ZEND_ACC_CLOSURE`,
/// which a reflector answers yes to when it keeps a closure, php's
/// `intern->obj`.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->isClosure());
/// var_dump((new ReflectionFunction(function () {}))->isClosure());
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// ```
pub(super) fn reflection_function_is_closure(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (function, _) = this_function(req)?;
    Ok(Value::bool(matches!(function, FunctionRef::Closure(_))))
}

/// `ReflectionFunction::isAnonymous()`: `ZEND_ACC_CLOSURE` again, on
/// the subclass.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->isAnonymous());
/// var_dump((new ReflectionFunction(fn () => 1))->isAnonymous());
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// ```
pub(super) fn reflection_function_is_anonymous(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (function, _) = this_function(req)?;
    Ok(Value::bool(matches!(function, FunctionRef::Closure(_))))
}

/// `ReflectionFunctionAbstract::getClosureThis()`: the receiver the
/// closure was bound to, `zend_get_closure_this_ptr` over
/// `intern->obj`, one more owner for the caller; `null` for a closure
/// with none and for a reflector that keeps no closure.
///
/// ```php
/// class C { public $p = 7; function m() { return function () { return $this->p; }; } }
/// $bound = (new C)->m();
/// var_dump((new ReflectionFunction($bound))->getClosureThis());
/// var_dump((new ReflectionFunction(function () {}))->getClosureThis());
/// var_dump((new ReflectionFunction('strlen'))->getClosureThis());
/// ```
/// ```expect
/// object(C)#1 (1) {
///   ["p"]=>
///   int(7)
/// }
/// NULL
/// NULL
/// ```
pub(super) fn reflection_function_get_closure_this(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (function, _) = this_function(req)?;
    let FunctionRef::Closure(closure) = function else {
        return Ok(Value::null());
    };
    let Some(this) = closure.closure(req).and_then(|info| info.this) else {
        return Ok(Value::null());
    };
    let value = this.as_value();
    req.addref(&value);
    Ok(value)
}

/// `ReflectionFunctionAbstract::getClosureScopeClass()`:
/// `closure_func->common.scope` as a `ReflectionClass`, which is the
/// class whose body the closure was written in; `null` for one written
/// outside a class and for a reflector that keeps no closure.
///
/// ```php
/// class C { function m() { return function () { return 1; }; } }
/// $in = (new C)->m();
/// var_dump((new ReflectionFunction($in))->getClosureScopeClass()->getName());
/// var_dump((new ReflectionFunction(function () {}))->getClosureScopeClass());
/// var_dump((new ReflectionFunction('strlen'))->getClosureScopeClass());
/// ```
/// ```expect
/// string(1) "C"
/// NULL
/// NULL
/// ```
pub(super) fn reflection_function_get_closure_scope_class(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (function, _) = this_function(req)?;
    let FunctionRef::Closure(closure) = function else {
        return Ok(Value::null());
    };
    let Some(scope) = closure.closure(req).and_then(|info| info.scope) else {
        return Ok(Value::null());
    };
    let object = reflection_class_factory(req, scope)?;
    Ok(object.as_value())
}

/// `ReflectionFunctionAbstract::getClosureUsedVariables()`: what the
/// closure bound, by name, in the order the table holds them, which is
/// [`php_ext::values::PhpObject::closure_bound`]; the empty array for
/// a reflector that keeps no closure. An arrow function's captures are
/// in it as a `use` list's are, since both are one table.
///
/// A name bound by reference stands for the cell, as php's bucket
/// does, and php's `var_dump` prints such an element with a `&` before
/// its value; this engine's dereferences first and prints no `&`
/// (#449), so what a witness here can pin is the binding by value.
///
/// ```php
/// $b = 1;
/// $f = function () use ($b) { return $b; };
/// var_dump((new ReflectionFunction($f))->getClosureUsedVariables());
/// var_dump((new ReflectionFunction(fn () => $b))->getClosureUsedVariables());
/// var_dump((new ReflectionFunction('strlen'))->getClosureUsedVariables());
/// ```
/// ```expect
/// array(1) {
///   ["b"]=>
///   int(1)
/// }
/// array(1) {
///   ["b"]=>
///   int(1)
/// }
/// array(0) {
/// }
/// ```
pub(super) fn reflection_function_get_closure_used_variables(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (function, _) = this_function(req)?;
    let mut used = PhpArray::new(req)?;
    let FunctionRef::Closure(closure) = function else {
        return Ok(Value::array(used));
    };
    for (name, value) in closure.closure_bound(req)? {
        req.addref(&value);
        used.set(req, Key::Str(name), value)?;
    }
    Ok(Value::array(used))
}

/// `ReflectionFunctionAbstract::getAttributes(?string $name = null,
/// int $flags = 0)`: `reflect_attributes` over
/// `fptr->common.attributes`, with `ZEND_ATTRIBUTE_TARGET_METHOD` where
/// the function has a scope and `ZEND_ATTRIBUTE_TARGET_FUNCTION` where
/// it has none.
///
/// ```php
/// var_dump((new ReflectionFunction('json_encode'))->getAttributes());
/// $m = new ReflectionMethod('ReflectionFunction', 'isDisabled');
/// var_dump(count($m->getAttributes()), count($m->getAttributes('Deprecated')), count($m->getAttributes('deprecated')), count($m->getAttributes('Other')));
/// ```
/// ```expect
/// array(0) {
/// }
/// int(1)
/// int(1)
/// int(1)
/// int(0)
/// ```
pub(super) fn reflection_function_get_attributes(req: &mut Request) -> Result<Value, Thrown> {
    let (function, decl) = this_function(req)?;
    let (scope, target) = match function {
        FunctionRef::Method(scope, _) => (Some(scope), super::attr::TARGET_METHOD),
        _ => (None, super::attr::TARGET_FUNCTION),
    };
    let attributes = decl.attributes.clone();
    super::attr::reflect_attributes(req, &attributes, scope, target)
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
/// class M extends ReflectionMethod {}
/// var_dump(get_class(M::createFromMethodName('Exception::getCode')));
/// ```
/// ```expect
/// string(7) "getCode"
/// string(9) "Exception"
/// string(1) "M"
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
    // `object_init_ex(return_value, execute_data->This.value.ce)`: an
    // object of the class the static call named, so a subclass of
    // `ReflectionMethod` gets its own class back.
    let reflector = match req.called_scope() {
        Some(called) => called,
        None => class_named(req, b"ReflectionMethod")?,
    };
    let object = new_object(req, reflector)?;
    let scope = class_name_of(req, found.scope);
    set_name(req, object, &found.decl.function.name.0)?;
    set_string_prop(req, object, b"class", &scope)?;
    super::keep(req, object, class, None);
    Ok(object.as_value())
}

/// `ZEND_PARSE_PARAMETERS_START(n, -1)`'s count check: php's
/// `ArgumentCountError`, `expects at least N argument(s), M given`.
fn at_least_arguments(req: &mut Request, least: usize) -> Result<(), Thrown> {
    let count = req.argument_count();
    if count >= least {
        return Ok(());
    }
    let plural = if least == 1 { "" } else { "s" };
    Err(arg_count_error(
        req,
        &format!("at least {least} argument{plural}, {count} given"),
    ))
}

/// `Z_PARAM_VARIADIC` from `position` on: the call's remaining
/// arguments as they stand on the frame, each a view the served call
/// takes one more owner of.
fn variadic_arguments(req: &mut Request, from: usize) -> Vec<Value> {
    (from..=req.argument_count())
        .filter_map(|position| {
            let at = u32::try_from(position).ok()?;
            Some(req.argument(at)?.deref())
        })
        .collect()
}

/// `Z_PARAM_ARRAY` at `position`, the `h` of `invokeArgs`.
fn array_argument(req: &mut Request, position: u32) -> Result<PhpArray, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let given = php_ext::coerce::coerce_param(req, &given, &php_ext::decl::Type::Array, position)?;
    match given.as_array() {
        Some(array) => Ok(*array),
        None => PhpArray::new(req),
    }
}

/// `zend_call_known_fcc` over the function a `ReflectionFunction`
/// holds, which is what `ReflectionFunction::invoke` ends in: the
/// callable its name or closure resolves to through
/// `zend_is_callable_ex`, served by `serve` with the arguments the call
/// was given, and the value it was read off, one owner of the caller's,
/// given up after the call as `zend_fcc_dtor` gives its copy up. What
/// names no callee is php's `ReflectionException`, the `Invocation of
/// ... failed` php raises when the call came back with nothing and left
/// nothing pending; a method is [`invoke_method`]'s and never reaches
/// here.
fn invoke(
    req: &mut Request,
    function: &FunctionRef,
    serve: impl FnOnce(&mut Request, &Callable) -> Result<Value, Thrown>,
) -> Result<Value, Thrown> {
    let value = match function {
        FunctionRef::Method(..) | FunctionRef::Invoke(_) => {
            return Err(invocation_failed(req, function));
        }
        FunctionRef::Closure(object) => {
            let value = object.as_value();
            req.addref(&value);
            value
        }
        FunctionRef::Global(name) => string_value(req, name)?,
    };
    let served = if is_callable(req, &value) {
        to_callable(req, &value).and_then(|callable| serve(req, &callable))
    } else {
        Err(invocation_failed(req, function))
    };
    req.release(value);
    served
}

/// `zend_call_known_function` over the method a `ReflectionMethod`
/// holds, which is what `reflection_method_invoke` ends in: the `fcc`
/// php fills by hand, the method `decl` names on `class`, the
/// reflector's class, as the called scope and on the receiver given,
/// with no visibility held against the caller ([`known_method`]),
/// served by `serve`. A method the class does not hold is php's
/// `Invocation of method ... failed`, named on `scope`, the class that
/// declares it, as php names `mptr->common.scope`.
fn invoke_method(
    req: &mut Request,
    class: ClassId,
    decl: &MethodDecl,
    scope: ClassId,
    this: Option<PhpObject>,
    serve: impl FnOnce(&mut Request, &Callable) -> Result<Value, Thrown>,
) -> Result<Value, Thrown> {
    // A closure's `__invoke` is no row of the table: php serves the
    // handler, `Closure::__invoke`, on the closure given, which is the
    // closure called.
    let lowered = decl.function.name.0.to_ascii_lowercase();
    if let Some(this) = this
        && super::is_closure_invoke(req, scope, &lowered)
    {
        let callable = to_callable(req, &this.as_value())?;
        return serve(req, &callable);
    }
    let Some(callable) = known_method(req, class, &decl.function.name, this) else {
        let held = FunctionRef::Method(scope, decl.function.name.0.to_vec());
        return Err(invocation_failed(req, &held));
    };
    serve(req, &callable)
}

/// php's `ReflectionException` for a call that named no callee.
fn invocation_failed(req: &mut Request, function: &FunctionRef) -> Thrown {
    let what = invocation_of(req, function);
    throw(
        req,
        "ReflectionException",
        &format!("Invocation of {what} failed"),
    )
}

/// php's two `Invocation of %s failed` subjects: `function name()` and
/// `method Class::name()`.
fn invocation_of(req: &mut Request, function: &FunctionRef) -> String {
    match function {
        FunctionRef::Global(name) => {
            format!("function {}()", String::from_utf8_lossy(name))
        }
        FunctionRef::Method(scope, name) => format!(
            "method {}::{}()",
            String::from_utf8_lossy(&class_name_of(req, *scope)),
            String::from_utf8_lossy(name)
        ),
        FunctionRef::Closure(_) => String::from("function {closure}()"),
        FunctionRef::Invoke(_) => String::from("method Closure::__invoke()"),
    }
}

/// `ReflectionFunction::invoke(mixed ...$args): mixed`,
/// `ZEND_METHOD(ReflectionFunction, invoke)`: the function the
/// reflector names served with the arguments as passed, the extra
/// named ones handed on as `Z_PARAM_VARIADIC_WITH_NAMED` hands them,
/// and its return handed back. A reflector over a closure serves the
/// closure, php's `get_closure` on `intern->obj`.
///
/// ```php
/// var_dump((new ReflectionFunction('strtoupper'))->invoke('ab'));
/// function pair($a, $b) { return "$a|$b"; }
/// var_dump((new ReflectionFunction('pair'))->invoke(1, 2));
/// $f = function ($n) { return $n * 2; };
/// var_dump((new ReflectionFunction($f))->invoke(21));
/// ```
/// ```expect
/// string(2) "AB"
/// string(3) "1|2"
/// int(42)
/// ```
pub(super) fn reflection_function_invoke(req: &mut Request) -> Result<Value, Thrown> {
    let (function, _) = this_function(req)?;
    let arguments = variadic_arguments(req, 1);
    let named = req.named_arguments();
    invoke(req, &function, |req, callable| {
        call_named(req, callable, &arguments, &named)
    })
}

/// `ReflectionFunction::invokeArgs(array $args): mixed`,
/// `ZEND_METHOD(ReflectionFunction, invokeArgs)`: the same call with
/// the array as the whole argument list, which php hands
/// `zend_call_known_fcc` as `named_params` and its walk reads
/// positionally for every integer key, `call_array`.
///
/// ```php
/// var_dump((new ReflectionFunction('str_repeat'))->invokeArgs(['ab', 2]));
/// var_dump((new ReflectionFunction('str_repeat'))->invokeArgs(['times' => 3, 'string' => 'c']));
/// ```
/// ```expect
/// string(4) "abab"
/// string(3) "ccc"
/// ```
pub(super) fn reflection_function_invoke_args(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 1)?;
    let (function, _) = this_function(req)?;
    let array = array_argument(req, 1)?;
    invoke(req, &function, |req, callable| {
        call_array(req, callable, array)
    })
}

/// The receiver `reflection_method_invoke` passes: none for a method
/// the class declares on itself, whatever was given, since php ignores
/// the first argument there; and for an instance method the object
/// given, held to being an instance of the class that declared the
/// method. php's two `ReflectionException`s are the object missing and
/// the object of another class; before either, `o!` holds the argument
/// to `?object` whatever the method is.
///
/// ```php
/// class M { static function s() { echo "s\n"; } }
/// try { (new ReflectionMethod('M', 's'))->invoke('x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// (new ReflectionMethod('M', 's'))->invoke(null);
/// ```
/// ```expect
/// ReflectionMethod::invoke(): Argument #1 ($object) must be of type ?object, string given
/// s
/// ```
fn receiver(
    req: &mut Request,
    decl: &MethodDecl,
    scope: ClassId,
    position: u32,
) -> Result<Option<PhpObject>, Thrown> {
    if let Some(given) = req.argument(position) {
        let ty = php_ext::decl::Type::Nullable(Box::new(php_ext::decl::Type::Object));
        php_ext::coerce::coerce_param(req, &given, &ty, position)?;
    }
    if !decl.is_instance {
        return Ok(None);
    }
    let given = req.argument(position).unwrap_or_default().deref();
    let Some(object) = given.as_object() else {
        let class = class_name_of(req, scope);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Trying to invoke non static method {}::{}() without an object",
                String::from_utf8_lossy(&class),
                String::from_utf8_lossy(&decl.function.name.0)
            ),
        ));
    };
    if !object.instance_of(req, scope) {
        return Err(throw(
            req,
            "ReflectionException",
            "Given object is not an instance of the class this method was declared in",
        ));
    }
    Ok(Some(*object))
}

/// php's first refusal in `reflection_method_invoke`, before the
/// parameters are read at all: an abstract method has no body to enter.
fn abstract_refused(req: &mut Request, decl: &MethodDecl, scope: ClassId) -> Result<(), Thrown> {
    if !decl.is_abstract {
        return Ok(());
    }
    let class = class_name_of(req, scope);
    Err(throw(
        req,
        "ReflectionException",
        &format!(
            "Trying to invoke abstract method {}::{}()",
            String::from_utf8_lossy(&class),
            String::from_utf8_lossy(&decl.function.name.0)
        ),
    ))
}

/// `ReflectionMethod::invoke(?object $object, mixed ...$args): mixed`,
/// `reflection_method_invoke` with `variadic` set: the method served on
/// the object given, or on no receiver where the class declares the
/// method on itself.
///
/// ```php
/// class Greet {
///     public function hello($who) { return "hello $who"; }
///     public static function shout($who) { return strtoupper($who); }
///     private function secret() { return 'shh'; }
/// }
/// $greet = new Greet();
/// var_dump((new ReflectionMethod('Greet', 'hello'))->invoke($greet, 'world'));
/// var_dump((new ReflectionMethod('Greet', 'shout'))->invoke(null, 'ab'));
/// var_dump((new ReflectionMethod('Greet', 'secret'))->invoke($greet));
/// try { (new ReflectionMethod('Greet', 'hello'))->invoke(null); }
/// catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(11) "hello world"
/// string(2) "AB"
/// string(3) "shh"
/// Trying to invoke non static method Greet::hello() without an object
/// ```
pub(super) fn reflection_method_invoke(req: &mut Request) -> Result<Value, Thrown> {
    let (class, decl, scope) = this_method(req)?;
    abstract_refused(req, &decl, scope)?;
    at_least_arguments(req, 1)?;
    let this = receiver(req, &decl, scope, 1)?;
    let arguments = variadic_arguments(req, 2);
    let named = req.named_arguments();
    invoke_method(req, class, &decl, scope, this, |req, callable| {
        call_named(req, callable, &arguments, &named)
    })
}

/// `ReflectionMethod::invokeArgs(?object $object, array $args): mixed`,
/// `reflection_method_invoke` with `variadic` clear: the same call with
/// the array as the whole argument list.
///
/// ```php
/// class Sum { public function of($a, $b) { return $a + $b; } }
/// var_dump((new ReflectionMethod('Sum', 'of'))->invokeArgs(new Sum(), [2, 3]));
/// var_dump((new ReflectionMethod('Sum', 'of'))->invokeArgs(new Sum(), ['b' => 4, 'a' => 1]));
/// ```
/// ```expect
/// int(5)
/// int(5)
/// ```
///
/// The method is the one the reflector holds, served as php fills
/// `fcc` by hand and not resolved through `zend_is_callable_ex`: a
/// private or protected method is served from outside its class, and
/// one the class declares on itself with no receiver (#733).
///
/// ```php
/// class Vault {
///     private function open($a, $b) { return "$a$b"; }
///     protected static function peek($x) { return "[$x]"; }
/// }
/// var_dump((new ReflectionMethod('Vault', 'open'))->invokeArgs(new Vault(), ['b' => 2, 'a' => 1]));
/// var_dump((new ReflectionMethod('Vault', 'peek'))->invokeArgs(null, [3]));
/// ```
/// ```expect
/// string(2) "12"
/// string(3) "[3]"
/// ```
///
/// What is no array is php's `TypeError`, whose tail names the value as
/// `zend_zval_value_name` does and not as `zend_zval_type_name` does, so
/// `true` stands where the type alone would read `bool` (#462):
///
/// ```php
/// (new ReflectionMethod('Exception', 'getCode'))->invokeArgs(new Exception, true);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: ReflectionMethod::invokeArgs(): Argument #2 ($args) must be of type array, true given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionMethod->invokeArgs(Object(Exception), true)
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(super) fn reflection_method_invoke_args(req: &mut Request) -> Result<Value, Thrown> {
    let (class, decl, scope) = this_method(req)?;
    abstract_refused(req, &decl, scope)?;
    exactly_arguments(req, 2)?;
    let this = receiver(req, &decl, scope, 1)?;
    let array = array_argument(req, 2)?;
    invoke_method(req, class, &decl, scope, this, |req, callable| {
        call_array(req, callable, array)
    })
}
