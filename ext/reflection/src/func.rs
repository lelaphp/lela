//! The accessors `php_reflection.c` answers off one `zend_function`:
//! `ReflectionFunctionAbstract`'s, which a `ReflectionFunction` and a
//! `ReflectionMethod` both inherit, and the modifier and scope
//! accessors `ReflectionMethod` adds on top. Each reads the
//! declaration the reflector's `$name`, and a method's `$class`, look
//! up again, through [`super::signature::this_function`].
//!
//! `ReflectionGenerator` and `ReflectionFiber` read the execution the
//! object they keep stands at ([`Request::execution`]), one view for
//! both.

use php_ext::abi::{Callable, Instance, NamedVariadic, OrFalse, Raw, Static, This, Union};
use php_ext::attr::php_class;
use php_ext::call::{
    bound_variables, call_array, call_named, closure_from_callable, known_method, lookup_function,
};
use php_ext::coerce::{is_callable, to_callable};
use php_ext::decl::{FunctionDecl, MethodDecl, Name, Visibility};
use php_ext::raising::{
    Thrown, arg_count_error, argument_error, deprecated, fatal, throw, throw_error,
};
use php_ext::request::{Execution, Request};
use php_ext::values::{ClassId, Owner, PhpArray, PhpObject, PhpString, Value};

use super::signature::{FunctionRef, parameter_factory, this_function, type_factory};
use super::{
    HeldMethod, at_most_arguments, class_name_of, class_named, closure_string, exactly_arguments,
    function_string, is_constructor, keep, keep_closure, keep_method, kept_class, kept_closure,
    kept_method, kept_object, method_or_invoke, method_string, namespace_end, new_object,
    reflection_class_factory, required_parameters, set_name, set_string_prop, signature,
    string_value, this_name,
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

/// A hook's reflector holds the hook itself, found among the hooks of
/// the property its name names in `class`, with the class that wrote
/// it; none for the reflector of any other method.
fn kept_hook_method(
    req: &mut Request,
    this: PhpObject,
    class: ClassId,
    name: &[u8],
) -> Option<(ClassId, MethodDecl)> {
    if !super::kept_hook(req, this) {
        return None;
    }
    let (property, kind) = super::member::hook_name(name)?;
    php_ext::call::property_hook(req, class, property, kind)
}

/// The method a `ReflectionMethod` reflects, with the class its
/// `$class` names: php's `GET_REFLECTION_OBJECT_PTR(mptr)` beside
/// `intern->ce`.
fn this_method(
    req: &mut Request,
    this: PhpObject,
) -> Result<(ClassId, MethodDecl, ClassId), Thrown> {
    if let Some(held) = kept_method(req, this)
        && let Some(method) = super::LinkedMethod::of_held(req, held)
    {
        return Ok((method.scope, method.decl, method.scope));
    }
    let name = this_name(req, this, "ReflectionMethod")?;
    let Some(class) = req
        .this()
        .and_then(|this| this.prop(req, b"class"))
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Err(throw_error(
            req,
            "Error",
            "Typed property ReflectionMethod::$class must not be accessed before initialization",
        ));
    };
    let class = class_named(req, &class)?;
    if let Some((scope, decl)) = kept_hook_method(req, this, class, &name) {
        return Ok((class, decl, scope));
    }
    let kept = kept_closure(req, this);
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

/// Whether the function is php's `ZEND_INTERNAL_FUNCTION`: a native,
/// whose declaration names no file, as `_function_string` reads it for
/// `<internal`.
pub(super) fn is_internal(decl: &FunctionDecl) -> bool {
    decl.file.0.is_empty()
}

/// One method of a linked class by its name, `zend_hash_find_ptr` over
/// the function table.
fn method_decl(req: &mut Request, class: ClassId, name: &[u8]) -> Option<MethodDecl> {
    let row = req.method_row(class, name)?;
    req.row_method(row).cloned()
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

/// The extension that declared the function, `internal->module`: for a
/// method the one named by the function its method row holds, the
/// declaring class's, or for a native trait's method copied into a
/// class the trait's.
fn function_extension_name(req: &mut Request, this: PhpObject) -> Result<Option<String>, Thrown> {
    let (function, decl) = this_function(req, this)?;
    if !is_internal(&decl) {
        return Ok(None);
    }
    match function {
        // A closure is a user function, which `is_internal` answered
        // for above, so nothing reaches here.
        // A closure's `__invoke` is made with a null `module`.
        FunctionRef::Closure(_) | FunctionRef::Invoke(_) => Ok(None),
        FunctionRef::Method(class, name) => Ok(req
            .method_row(class, &name)
            .and_then(|row| req.row_function(row))
            .and_then(|found| req.function_extension(found))
            .and_then(|e| req.extension_name(e))
            .map(str::to_owned)),
        FunctionRef::Global(name) => {
            let found = super::lookup_function(req, &name);
            Ok(found
                .and_then(|found| req.function_extension(found))
                .and_then(|e| req.extension_name(e))
                .map(str::to_owned))
        }
    }
}

// ------------------------------------------------- ReflectionMethod

/// `mptr->common.prototype`'s scope for the method `$name` names in
/// the table of `intern->ce`, the class the reflector was asked about:
/// an inherited method an interface of that class names is its own row
/// there, whose prototype `do_inheritance_check_on_method` set to the
/// interface's (`SEPARATE_METHOD`), where the declaring class's row has
/// none.
///
/// ```php
/// interface a { function foo(); function bar(); }
/// interface b { function foo(); }
/// abstract class c { function bar() { } }
/// class x extends c implements a, b { function foo() { } }
/// var_dump((new ReflectionMethod("x", "foo"))->getPrototype()->class);
/// var_dump((new ReflectionMethod("x", "bar"))->getPrototype()->class);
/// var_dump((new ReflectionMethod("c", "bar"))->hasPrototype());
/// ```
/// ```expect
/// string(1) "b"
/// string(1) "a"
/// bool(false)
/// ```
fn prototype_of(
    req: &mut Request,
    this: PhpObject,
    name: &[u8],
) -> Result<Option<ClassId>, Thrown> {
    match kept_method(req, this) {
        Some(HeldMethod::Row(held)) => return Ok(req.row_prototype(held)),
        // The trampoline php makes for a closure's `__invoke` has none.
        Some(HeldMethod::Invoke(_)) => return Ok(None),
        None => {}
    }
    let named = (this.prop(req, b"class")).and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
    let class = match (super::kept_class(req, this), named) {
        (Some(class), _) => class,
        (None, Some(named)) => class_named(req, &named)?,
        (None, None) => return Ok(None),
    };
    Ok(req
        .method_row(class, name)
        .and_then(|row| req.row_prototype(row)))
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
    serve: impl FnOnce(&mut Request, &Callable) -> Result<Owner<Value>, Thrown>,
) -> Result<Owner<Value>, Thrown> {
    let value = match function {
        FunctionRef::Method(..) | FunctionRef::Invoke(_) => {
            return Err(invocation_failed(req, function));
        }
        FunctionRef::Closure(object) => Owner::take(req, object.as_value()),
        FunctionRef::Global(name) => string_value(req, name)?,
    };
    let served = if is_callable(req, &value) {
        to_callable(req, &value).and_then(|callable| serve(req, &callable))
    } else {
        Err(invocation_failed(req, function))
    };
    value.give_up(req);
    served
}

/// `zend_call_known_function` over the method a `ReflectionMethod`
/// holds, which is what `reflection_method_invoke` ends in: the `fcc`
/// php fills by hand, the method `decl` names on `class`, `intern->ce`,
/// the class the reflector was asked about, as the called scope
/// (`fcc.called_scope = intern->ce`) and on the receiver given,
/// with no visibility held against the caller ([`known_method`]),
/// served by `serve`. A method the class does not hold is php's
/// `Invocation of method ... failed`, named on `scope`, the class that
/// declares it, as php names `mptr->common.scope`; one only `scope`
/// holds, a parent's private method, is served on `scope`.
///
/// ```php
/// abstract class A { const WHAT = 'A'; static function call() { echo static::WHAT; } }
/// class B extends A { const WHAT = 'B'; }
/// ReflectionMethod::createFromMethodName("b::call")->invoke(null);
/// (new ReflectionMethod('B', 'call'))->invokeArgs(null, []);
/// (new ReflectionMethod('A', 'call'))->invoke(null);
/// ```
/// ```expect
/// BBA
/// ```
fn invoke_method(
    req: &mut Request,
    class: ClassId,
    decl: &MethodDecl,
    scope: ClassId,
    this: Option<PhpObject>,
    serve: impl FnOnce(&mut Request, &Callable) -> Result<Owner<Value>, Thrown>,
) -> Result<Owner<Value>, Thrown> {
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
    let name = &decl.function.name;
    let found =
        known_method(req, class, name, this).or_else(|| known_method(req, scope, name, this));
    let Some(callable) = found else {
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
    given: Raw<Option<PhpObject>>,
) -> Result<Option<PhpObject>, Thrown> {
    let given = given.hold(req)?;
    if !decl.is_instance {
        return Ok(None);
    }
    let Some(object) = given else {
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
    Ok(Some(object))
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

/// `ReflectionFunctionAbstract` as the stub declares it.
#[php_class(abstract, implements(crate::Reflector), not_serializable, size = 96)]
pub(crate) struct ReflectionFunctionAbstract;

#[php_class]
impl ReflectionFunctionAbstract {
    #[php(property)]
    const name: PhpString;

    /// `private function __clone(): void`, which every reflector's clone
    /// shares: a clone is refused by the object handlers, and a call
    /// reached through reflection anyway is php's `ReflectionException`,
    /// as `_DO_THROW` in each `__clone` body raises it.
    ///
    /// ```php
    /// echo new ReflectionMethod('ReflectionClass', '__clone');
    /// try { (new ReflectionMethod('ReflectionClass', '__clone'))->invoke(new ReflectionClass('stdClass')); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Method [ <internal:Reflection> private method __clone ] {
    ///
    ///   - Parameters [0] {
    ///   }
    ///   - Return [ void ]
    /// }
    /// Cannot clone object using __clone()
    /// ```
    #[php(private, arity = manual)]
    fn __clone(req: &mut Request, _: This<Self>) -> Result<(), Thrown> {
        Err(throw(
            req,
            "ReflectionException",
            "Cannot clone object using __clone()",
        ))
    }

    /// `ReflectionFunctionAbstract::inNamespace()`: whether the name has a
    /// `\` in it, and `false` for a closure, whose `{closure:..}` name is
    /// in no namespace whatever the one it was written in; the same holds
    /// `getNamespaceName()` to the empty string and `getShortName()` to
    /// the whole name.
    ///
    /// ```php
    /// var_dump((new ReflectionFunction('json_encode'))->inNamespace());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    ///
    /// ```php
    /// namespace Foo;
    /// class Bar { function baz() { return function () {}; } }
    /// $r = new \ReflectionFunction((new Bar)->baz());
    /// var_dump($r->getShortName(), $r->getNamespaceName(), $r->inNamespace());
    /// ```
    /// ```expect
    /// string(26) "{closure:Foo\Bar::baz():3}"
    /// string(0) ""
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn inNamespace(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (function, decl) = this_function(req, this.object())?;
        if is_anonymous(req, &function) {
            return Ok(false);
        }
        Ok(namespace_end(&decl.name.0).is_some())
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
    #[php(tentative)]
    fn isClosure(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (function, _) = this_function(req, this.object())?;
        Ok(matches!(function, FunctionRef::Closure(_)))
    }

    /// `ReflectionFunctionAbstract::isDeprecated()`:
    /// `_function_check_flag(ZEND_ACC_DEPRECATED)`, the bit the
    /// `#[\Deprecated]` attribute sets on a native's or a user function's
    /// declaration, read from the attribute as `_function_string` reads it
    /// for `, deprecated`. `ctype` deprecates none of its eleven.
    ///
    /// ```php
    /// var_dump((new ReflectionFunction('ctype_alpha'))->isDeprecated());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    ///
    /// A method's and a closure's bit are their own declaration's, so an
    /// interface's deprecated method is deprecated and its implementation
    /// is not:
    ///
    /// ```php
    /// interface I { #[\Deprecated] function t(); }
    /// class C implements I { function t() {} #[\Deprecated] static function s() {} }
    /// var_dump((new ReflectionMethod('I', 't'))->isDeprecated(), (new ReflectionMethod('C', 't'))->isDeprecated(), (new ReflectionMethod('C', 's'))->isDeprecated());
    /// var_dump((new ReflectionFunction(#[\Deprecated] function () {}))->isDeprecated(), (new ReflectionFunction(fn () => 1))->isDeprecated());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isDeprecated(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, decl) = this_function(req, this.object())?;
        Ok(decl
            .attributes
            .iter()
            .any(|a| a.name.0.eq_ignore_ascii_case(b"Deprecated")))
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
    #[php(tentative)]
    fn isInternal(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, decl) = this_function(req, this.object())?;
        Ok(is_internal(&decl))
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
    #[php(tentative)]
    fn isUserDefined(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, decl) = this_function(req, this.object())?;
        Ok(!is_internal(&decl))
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
    #[php(tentative)]
    fn isGenerator(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, decl) = this_function(req, this.object())?;
        Ok(decl.is_generator)
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
    #[php(tentative)]
    fn isVariadic(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, decl) = this_function(req, this.object())?;
        Ok(decl.parameters.iter().any(|p| p.variadic))
    }

    /// `ReflectionFunctionAbstract::isStatic()`: `ZEND_ACC_STATIC`, which
    /// a function outside a class never carries and a closure carries where
    /// it was written `static` or made of a `static` method.
    ///
    /// ```php
    /// var_dump((new ReflectionMethod('ReflectionMethod', 'createFromMethodName'))->isStatic());
    /// var_dump((new ReflectionFunction('json_encode'))->isStatic(), (new ReflectionFunction(static fn () => 1))->isStatic());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isStatic(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (function, _) = this_function(req, this.object())?;
        if let FunctionRef::Closure(closure) = &function {
            let is_static = closure
                .closure(req)
                .is_some_and(|info| req.closure_refuses_receiver(info));
            return Ok(is_static);
        }
        let FunctionRef::Method(class, name) = &function else {
            return Ok(false);
        };
        let (class, name) = (*class, name.clone());
        let decl = method_decl(req, class, &name);
        Ok(decl.is_some_and(|decl| !decl.is_instance))
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
    #[php(tentative)]
    fn getClosureThis(req: &mut Request, this: This<Self>) -> Result<Option<PhpObject>, Thrown> {
        let (function, _) = this_function(req, this.object())?;
        let FunctionRef::Closure(closure) = function else {
            return Ok(None);
        };
        let Some(this) = closure.closure(req).and_then(|info| info.this) else {
            return Ok(None);
        };
        Ok(Some(this))
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
    #[php(tentative)]
    fn getClosureScopeClass(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::class::ReflectionClass>>>, Thrown> {
        let (function, _) = this_function(req, this.object())?;
        let FunctionRef::Closure(closure) = function else {
            return Ok(None);
        };
        let Some(scope) = closure.closure(req).and_then(|info| info.scope) else {
            return Ok(None);
        };
        let object = reflection_class_factory(req, scope)?;
        Ok(Some(object.into_instance()))
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
    #[php(tentative)]
    fn getClosureCalledClass(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::class::ReflectionClass>>>, Thrown> {
        let (function, _) = this_function(req, this.object())?;
        let FunctionRef::Closure(closure) = function else {
            return Ok(None);
        };
        let class = closure
            .closure(req)
            .and_then(|info| info.called_scope.or(info.scope));
        match class {
            Some(class) => Ok(Some(reflection_class_factory(req, class)?.into_instance())),
            None => Ok(None),
        }
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
    fn getClosureUsedVariables(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let (function, _) = this_function(req, this.object())?;
        let mut used = PhpArray::new(req)?;
        let FunctionRef::Closure(closure) = function else {
            return Ok(used);
        };
        for (name, value) in closure.closure_bound(req)? {
            used.set(req, name.into_key(), Owner::take(req, value))?;
        }
        Ok(used)
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
    #[php(tentative)]
    fn getDocComment(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let (_, decl) = this_function(req, this.object())?;
        let Some(doc) = decl.doc_comment.clone() else {
            return Ok(OrFalse::False);
        };
        PhpString::new(req, &doc).map(OrFalse::Value)
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
    #[php(tentative)]
    fn getEndLine(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        let (_, decl) = this_function(req, this.object())?;
        if is_internal(&decl) {
            return Ok(OrFalse::False);
        }
        Ok(OrFalse::Value(i64::from(decl.end_line)))
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
    #[php(tentative)]
    fn getExtension(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::ReflectionExtension>>>, Thrown> {
        let Some(name) = function_extension_name(req, this.object())? else {
            return Ok(None);
        };
        if req.extension_named(name.as_bytes()).is_none() {
            return Ok(None);
        }
        let class = class_named(req, b"ReflectionExtension")?;
        let object = new_object(req, class)?;
        set_name(req, *object, name.as_bytes())?;
        Ok(Some(object.into_instance()))
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
    #[php(tentative)]
    fn getExtensionName(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let Some(name) = function_extension_name(req, this.object())? else {
            return Ok(OrFalse::False);
        };
        PhpString::new(req, name.as_bytes()).map(OrFalse::Value)
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
    #[php(tentative)]
    fn getFileName(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let (_, decl) = this_function(req, this.object())?;
        if is_internal(&decl) {
            return Ok(OrFalse::False);
        }
        let file = decl.file.0.to_vec();
        PhpString::new(req, &file).map(OrFalse::Value)
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
    #[php(tentative)]
    fn getName(req: &mut Request, this: This<Self>) -> Result<PhpString, Thrown> {
        let (_, decl) = this_function(req, this.object())?;
        let name = decl.name.0.to_vec();
        PhpString::interned(req, &name)
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
    #[php(tentative)]
    fn getNamespaceName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let (function, decl) = this_function(req, this.object())?;
        if is_anonymous(req, &function) {
            return PhpString::new(req, b"");
        }
        let name = decl.name.0.to_vec();
        let space = namespace_end(&name).map_or_else(Vec::new, |end| name[..end].to_vec());
        PhpString::new(req, &space)
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
    #[php(tentative)]
    fn getNumberOfParameters(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let (_, decl) = this_function(req, this.object())?;
        Ok(i64::try_from(decl.parameters.len()).unwrap_or(0))
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
    #[php(tentative)]
    fn getNumberOfRequiredParameters(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let (_, decl) = this_function(req, this.object())?;
        Ok(i64::try_from(required_parameters(&decl)).unwrap_or(0))
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
    #[php(tentative)]
    fn getParameters(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let (function, decl) = this_function(req, this.object())?;
        if decl.parameters.is_empty() {
            return Ok(Owner::take(req, PhpArray::empty()));
        }
        let mut parameters = PhpArray::new(req)?;
        for offset in 0..decl.parameters.len() {
            let object = parameter_factory(req, &function, &decl, offset)?;
            parameters.append(req, object.into_value())?;
        }
        Ok(parameters)
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
    #[php(tentative)]
    fn getShortName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let (function, decl) = this_function(req, this.object())?;
        let name = decl.name.0.to_vec();
        let start = if is_anonymous(req, &function) {
            0
        } else {
            namespace_end(&name).map_or(0, |end| end + 1)
        };
        let short = name[start..].to_vec();
        PhpString::new(req, &short)
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
    #[php(tentative)]
    fn getStartLine(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        let (_, decl) = this_function(req, this.object())?;
        if is_internal(&decl) {
            return Ok(OrFalse::False);
        }
        Ok(OrFalse::Value(i64::from(decl.line)))
    }

    /// `ReflectionFunctionAbstract::getStaticVariables()`: a copy of the
    /// function's `static` table, `zend_hash_copy` over
    /// `ZEND_MAP_PTR_GET(static_variables_ptr)` with `zval_add_ref`, which
    /// php lays down from the op array's starting values where no call has
    /// yet ([`php_ext::call::bound_variables`]); the empty array for a
    /// native. Before the first call a `static` reads the value it was
    /// declared with, or `null` where its initializer runs at the call;
    /// after it, the value the function left there. A closure's table is
    /// its own and holds its `use` variables first; a method a trait gave a
    /// class is that class's, and the trait's own is untouched by it.
    ///
    /// ```php
    /// function f() { static $x = 1, $y, $z = [1, 2], $o = PHP_INT_MAX + strlen("x"); $x++; return $x; }
    /// $r = new ReflectionFunction('f');
    /// var_dump($r->getStaticVariables());
    /// f();
    /// var_dump($r->getStaticVariables());
    /// var_dump((new ReflectionFunction('strlen'))->getStaticVariables());
    /// ```
    /// ```expect
    /// array(4) {
    ///   ["x"]=>
    ///   int(1)
    ///   ["y"]=>
    ///   NULL
    ///   ["z"]=>
    ///   array(2) {
    ///     [0]=>
    ///     int(1)
    ///     [1]=>
    ///     int(2)
    ///   }
    ///   ["o"]=>
    ///   NULL
    /// }
    /// array(4) {
    ///   ["x"]=>
    ///   int(2)
    ///   ["y"]=>
    ///   NULL
    ///   ["z"]=>
    ///   array(2) {
    ///     [0]=>
    ///     int(1)
    ///     [1]=>
    ///     int(2)
    ///   }
    ///   ["o"]=>
    ///   float(9.223372036854776E+18)
    /// }
    /// array(0) {
    /// }
    /// ```
    ///
    /// ```php
    /// trait T { function n() { static $n = 0; return ++$n; } }
    /// class A { use T; }
    /// $a = new A; $a->n(); $a->n();
    /// var_dump((new ReflectionMethod('A', 'n'))->getStaticVariables(), (new ReflectionMethod('T', 'n'))->getStaticVariables());
    /// $b = 5;
    /// $c = function () use ($b) { static $k = 'k'; };
    /// var_dump((new ReflectionFunction($c))->getStaticVariables());
    /// ```
    /// ```expect
    /// array(1) {
    ///   ["n"]=>
    ///   int(2)
    /// }
    /// array(1) {
    ///   ["n"]=>
    ///   int(0)
    /// }
    /// array(2) {
    ///   ["b"]=>
    ///   int(5)
    ///   ["k"]=>
    ///   string(1) "k"
    /// }
    /// ```
    #[php(tentative)]
    fn getStaticVariables(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let (function, _) = this_function(req, this.object())?;
        let callable = match &function {
            FunctionRef::Method(scope, name) => {
                known_method(req, *scope, &Name(name.as_slice().into()), None)
            }
            FunctionRef::Closure(object) => to_callable(req, &object.as_value()).ok(),
            FunctionRef::Global(name) => {
                let value = string_value(req, name)?;
                let callable = to_callable(req, &value).ok();
                value.give_up(req);
                callable
            }
            // A closure's `__invoke` is `zend_get_closure_invoke_method`'s
            // internal trampoline, which holds no static variables.
            FunctionRef::Invoke(_) => None,
        };
        let Some(callable) = callable else {
            return Ok(Owner::take(req, PhpArray::empty()));
        };
        let bound = bound_variables(req, &callable)?;
        if bound.is_empty() {
            return Ok(Owner::take(req, PhpArray::empty()));
        }
        let mut statics = PhpArray::new(req)?;
        for (name, value) in bound {
            // `zval_add_ref`: a reference the table alone holds is copied
            // out of, so a `static` read after its call is its value.
            let value = if value.as_php_ref().is_some() && value.refcount() == 1 {
                value.deref()
            } else {
                value
            };
            statics.set(req, name.into_key(), Owner::take(req, value))?;
        }
        Ok(statics)
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
    #[php(tentative)]
    fn returnsReference(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, decl) = this_function(req, this.object())?;
        Ok(decl.returns_by_ref)
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
    #[php(tentative)]
    fn hasReturnType(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (function, decl) = this_function(req, this.object())?;
        let tentative = tentative_return(req, &function);
        Ok(decl.return_type.is_some() && !tentative)
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
    #[php(tentative)]
    fn getReturnType(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::signature::ReflectionType>>>, Thrown> {
        let (function, decl) = this_function(req, this.object())?;
        let tentative = tentative_return(req, &function);
        let Some(ty) = decl.return_type.filter(|_| !tentative) else {
            return Ok(None);
        };
        let object = type_factory(req, &ty, true)?;
        Ok(Some(object.into_instance()))
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
    fn hasTentativeReturnType(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (function, decl) = this_function(req, this.object())?;
        let tentative = tentative_return(req, &function);
        Ok(decl.return_type.is_some() && tentative)
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
    fn getTentativeReturnType(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::signature::ReflectionType>>>, Thrown> {
        let (function, decl) = this_function(req, this.object())?;
        let tentative = tentative_return(req, &function);
        let Some(ty) = decl.return_type.filter(|_| tentative) else {
            return Ok(None);
        };
        let object = type_factory(req, &ty, true)?;
        Ok(Some(object.into_instance()))
    }

    /// `ReflectionFunctionAbstract::getAttributes(?string $name = null,
    /// int $flags = 0)`: `reflect_attributes` over
    /// `fptr->common.attributes`, with `ZEND_ATTRIBUTE_TARGET_METHOD` where
    /// the function has a scope and is no closure the program wrote, a
    /// fake closure of a method among them, and
    /// `ZEND_ATTRIBUTE_TARGET_FUNCTION` otherwise. The arguments are
    /// evaluated in `fptr->common.scope`,
    /// which for a closure is the scope it is bound to:
    ///
    /// ```php
    /// class T1 { private const FOO = 'bar'; }
    /// class C1 {
    ///     private const FOO = 'foo';
    ///     static function f() { return #[A1(self::class, self::FOO)] function (#[A1(self::class)] $p) {}; }
    /// }
    /// $r = new ReflectionFunction(C1::f());
    /// echo implode(',', $r->getAttributes()[0]->getArguments()), ' ', implode(',', $r->getParameters()[0]->getAttributes()[0]->getArguments()), "\n";
    /// $r = new ReflectionFunction(C1::f()->bindTo(null, T1::class));
    /// echo implode(',', $r->getAttributes()[0]->getArguments()), "\n";
    /// ```
    /// ```expect
    /// C1,foo C1
    /// T1,bar
    /// ```
    ///
    /// ```php
    /// #[Attribute(Attribute::TARGET_METHOD)] class M {}
    /// class C { #[M] function m() {} }
    /// echo get_class((new ReflectionFunction((new C)->m(...)))->getAttributes()[0]->newInstance()), "\n";
    /// ```
    /// ```expect
    /// M
    /// ```
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
    fn getAttributes(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] name: Option<PhpString>,
        #[php(default = 0)] flags: i64,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let (function, decl) = this_function(req, this.object())?;
        // `fptr->common.scope`: a method's class, a closure's bound scope.
        let (scope, target) = match function {
            FunctionRef::Method(scope, _) => (Some(scope), super::attr::TARGET_METHOD),
            FunctionRef::Closure(closure) => {
                let info = closure.closure(req);
                let scope = info.as_ref().and_then(|info| info.scope);
                let fake = info.is_some_and(|info| info.fake);
                let target = if fake && scope.is_some() {
                    super::attr::TARGET_METHOD
                } else {
                    super::attr::TARGET_FUNCTION
                };
                (scope, target)
            }
            _ => (None, super::attr::TARGET_FUNCTION),
        };
        let hook = super::kept_hook(req, this.object());
        let attributes = decl.attributes.clone();
        let file = super::attr::declared_in(&decl.file.0);
        super::attr::reflect_attributes(
            req,
            &attributes,
            super::attr::Subject {
                hook,
                ..super::attr::Subject::new(scope, target, file.as_deref())
            },
            name,
            flags,
        )
    }
}

/// `ReflectionFunction` as the stub declares it.
#[php_class(extends(ReflectionFunctionAbstract), size = 96, payload = crate::Reflected, handlers = crate::HANDLERS)]
pub(crate) struct ReflectionFunction;

#[php_class]
impl ReflectionFunction {
    const IS_DEPRECATED: i64 = 2048;

    /// `ReflectionFunction::__construct(Closure|string $function)`: the
    /// function under the lowered name, a leading `\` dropped, its declared
    /// name kept as `$name`; a name the tables have not is php's
    /// `ReflectionException`, the name as given. A `Closure` is
    /// `zend_get_closure_method_def` over the object, which the reflector
    /// keeps as php keeps `intern->obj`, and `$name` is the
    /// `{closure:file:line}` the compiler built for it; an object of any
    /// other class is the type error.
    ///
    /// ```php
    /// $r = new ReflectionFunction(function ($a, $b = 1) {});
    /// var_dump($r->isClosure(), $r->getNumberOfParameters(), $r->getNumberOfRequiredParameters(), $r->isUserDefined());
    /// echo $r, "\n";
    /// ```
    /// ```expect
    /// bool(true)
    /// int(2)
    /// int(1)
    /// bool(true)
    /// Closure [ <user> function {closure:%s:2} ] {
    ///   @@ %s 2 - 2
    ///
    ///   - Parameters [2] {
    ///     Parameter #0 [ <required> $a ]
    ///     Parameter #1 [ <optional> $b = 1 ]
    ///   }
    /// }
    /// ```
    ///
    /// ```php
    /// new ReflectionFunction(new stdClass);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught TypeError: ReflectionFunction::__construct(): Argument #1 ($function) must be of type Closure|string, stdClass given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionFunction->__construct(Object(stdClass))
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// new ReflectionFunction('nope');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Function nope() does not exist in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionFunction->__construct('nope')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// `null` is `coerce_param`'s over `Closure|string`: the deprecation
    /// naming the union and the empty string written over the argument,
    /// which is what the trace of the miss prints, and under
    /// `strict_types` the `TypeError`.
    ///
    /// ```php
    /// new ReflectionFunction(null);
    /// ```
    /// ```expect exit=255
    /// Deprecated: ReflectionFunction::__construct(): Passing null to parameter #1 ($function) of type Closure|string is deprecated in %s on line 2
    ///
    /// Fatal error: Uncaught ReflectionException: Function () does not exist in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionFunction->__construct('')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// declare(strict_types=1);
    /// try { new ReflectionFunction(null); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// ReflectionFunction::__construct(): Argument #1 ($function) must be of type Closure|string, null given
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        function: Union<Instance<crate::Closure>, PhpString>,
    ) -> Result<(), Thrown> {
        let name = match function {
            Union::A(closure) => {
                let object = closure.object();
                // php's `IS_OBJECT` arm: `zend_get_closure_method_def` and
                // `intern->obj` holding the closure, `$name` the declaration's
                // own, which is the `{closure:…}` the compiler built.
                let Some(decl) = signature::closure_decl(req, object) else {
                    return Err(php_ext::raising::fatal(
                        req,
                        "InternalError: a Closure with no declaration behind it",
                    ));
                };
                let this = this.object();
                set_name(req, this, &decl.name.0)?;
                keep_closure(req, this, object);
                return Ok(());
            }
            Union::B(name) => name.as_bytes().to_vec(),
        };
        let Some(function) = lookup_function(req, &name) else {
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
            .function_decl(function)
            .map(|decl| decl.name.0.to_vec())
            .unwrap_or_default();
        let this = this.object();
        set_name(req, this, &declared)?;
        Ok(())
    }

    /// `ReflectionFunction::__toString()`: `_function_string` over the
    /// function `$name` names, no scope, indent empty.
    ///
    /// ```php
    /// echo new ReflectionFunction('ctype_alpha');
    /// ```
    /// ```expect
    /// Function [ <internal:ctype> function ctype_alpha ] {
    ///
    ///   - Parameters [1] {
    ///     Parameter #0 [ <required> mixed $text ]
    ///   }
    ///   - Return [ bool ]
    /// }
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let name = this_name(req, this.object(), "ReflectionFunction")?;
        if let Some(closure) = kept_closure(req, this.object()) {
            let text = closure_string(req, closure, "")?;
            return PhpString::new(req, text.as_bytes());
        }
        let Some(function) = lookup_function(req, &name) else {
            return Err(throw(
                req,
                "ReflectionException",
                &format!(
                    "Function {}() does not exist",
                    String::from_utf8_lossy(&name)
                ),
            ));
        };
        let text = function_string(req, function, "");
        PhpString::new(req, text.as_bytes())
    }

    /// `ReflectionFunction::isAnonymous()`: `ZEND_ACC_CLOSURE` without
    /// `ZEND_ACC_FAKE_CLOSURE`, so a closure made of a function that has a
    /// name, `strlen(...)`, is not anonymous.
    ///
    /// ```php
    /// var_dump((new ReflectionFunction('json_encode'))->isAnonymous());
    /// var_dump((new ReflectionFunction(fn () => 1))->isAnonymous());
    /// var_dump((new ReflectionFunction(strlen(...)))->isAnonymous(), (new ReflectionFunction(Closure::fromCallable('strlen')))->isAnonymous());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// ```
    fn isAnonymous(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (function, _) = this_function(req, this.object())?;
        Ok(is_anonymous(req, &function))
    }

    /// `ReflectionFunction::isDisabled()`: `false`, since a disabled
    /// function cannot be reflected at all, after the deprecation its
    /// declaration raises.
    ///
    /// ```php
    /// var_dump((new ReflectionFunction('strlen'))->isDisabled());
    /// ```
    /// ```expect
    /// Deprecated: Method ReflectionFunction::isDisabled() is deprecated since 8.0, as ReflectionFunction can no longer be constructed for disabled functions in %s on line 2
    /// bool(false)
    /// ```
    #[php(
        tentative,
        deprecated(
            since = "8.0",
            message = "as ReflectionFunction can no longer be constructed for disabled functions"
        )
    )]
    fn isDisabled(_: This<Self>) -> bool {
        false
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
    #[php(tentative)]
    fn invoke(
        req: &mut Request,
        this: This<Self>,
        args: NamedVariadic<Value>,
    ) -> Result<Owner<Value>, Thrown> {
        let (function, _) = this_function(req, this.object())?;
        let named = args.named().to_vec();
        let arguments = args.positional;
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
    #[php(tentative)]
    fn invokeArgs(
        req: &mut Request,
        this: This<Self>,
        args: PhpArray,
    ) -> Result<Owner<Value>, Thrown> {
        let (function, _) = this_function(req, this.object())?;
        invoke(req, &function, |req, callable| {
            call_array(req, callable, args)
        })
    }

    /// `ReflectionFunction::getClosure()`: the closure a reflector over one
    /// keeps, one more owner, since a closure is immutable; else
    /// `zend_create_fake_closure` over the function with no scope, which
    /// holds the function and not the name it was resolved by, so the
    /// name is given up once the closure is made.
    ///
    /// ```php
    /// $c = function () { return 1; };
    /// var_dump((new ReflectionFunction($c))->getClosure() === $c, (new ReflectionFunction('strlen'))->getClosure()('abc'));
    /// ```
    /// ```expect
    /// bool(true)
    /// int(3)
    /// ```
    #[php(tentative)]
    fn getClosure(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<crate::Closure>>, Thrown> {
        let (function, decl) = this_function(req, this.object())?;
        if let FunctionRef::Closure(closure) = function {
            return Ok(Owner::take(req, closure).into_instance());
        }
        let name = php_ext::values::PhpString::new(req, &decl.name.0)?.into_value();
        let made =
            to_callable(req, &name).and_then(|callable| closure_from_callable(req, &callable));
        name.give_up(req);
        Ok(made?.into_instance())
    }
}

/// `zend_create_fake_closure` over a method the reflector found, bound
/// to `object` where it is an instance method.
fn fake_closure(
    req: &mut Request,
    scope: ClassId,
    name: &Name,
    object: Option<PhpObject>,
) -> Result<Owner<Instance<crate::Closure>>, Thrown> {
    let Some(callable) = known_method(req, scope, name, object) else {
        return Err(fatal(
            req,
            "InternalError: a method the reflector found that its class does not serve",
        ));
    };
    closure_from_callable(req, &callable).map(Owner::into_instance)
}

/// The object a `ReflectionGenerator` or a `ReflectionFiber` keeps,
/// `intern->obj`, and its execution where it stands.
fn kept_execution(req: &mut Request, this: PhpObject) -> Result<(PhpObject, Execution), Thrown> {
    let execution = kept_object(req, this).and_then(|kept| Some((kept, req.execution(kept)?)));
    execution.ok_or_else(|| {
        throw_error(
            req,
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        )
    })
}

/// `REFLECTION_CHECK_VALID_GENERATOR`: the execution of a generator
/// that has a frame, and php's `ReflectionException` for a closed one.
fn standing_generator(
    req: &mut Request,
    this: PhpObject,
) -> Result<(PhpObject, Execution), Thrown> {
    let (kept, execution) = kept_execution(req, this)?;
    if execution.executing.is_none() {
        return Err(throw(
            req,
            "ReflectionException",
            "Cannot fetch information from a closed Generator",
        ));
    }
    Ok((kept, execution))
}

/// `REFLECTION_CHECK_VALID_FIBER`: the execution of a fiber that is
/// running or suspended, and php's `Error` for one not started or dead.
fn standing_fiber(req: &mut Request, this: PhpObject) -> Result<(PhpObject, Execution), Thrown> {
    let (kept, execution) = kept_execution(req, this)?;
    if execution.executing.is_none() {
        return Err(throw_error(
            req,
            "Error",
            "Cannot fetch information from a fiber that has not been started or is terminated",
        ));
    }
    Ok((kept, execution))
}

/// `zend_fetch_debug_backtrace` over the execution of `kept`, which
/// stands, alone.
fn trace_of(req: &mut Request, kept: PhpObject, options: i64) -> Result<Owner<PhpArray>, Thrown> {
    match req.execution_trace(kept, options)? {
        Some(trace) => Ok(trace),
        None => Err(fatal(
            req,
            "InternalError: an execution with a frame and no trace",
        )),
    }
}

/// `ReflectionGenerator` as the stub declares it.
#[php_class(final, not_serializable, size = 96, payload = crate::Reflected, handlers = crate::HANDLERS)]
pub(crate) struct ReflectionGenerator;

#[php_class]
impl ReflectionGenerator {
    /// `ReflectionGenerator::__construct(Generator $generator)`: the
    /// generator kept, closed or not.
    ///
    /// ```php
    /// function g() { yield 1; }
    /// $g = g(); $g->next();
    /// var_dump(new ReflectionGenerator($g));
    /// try { new ReflectionGenerator(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(ReflectionGenerator)#2 (0) {
    /// }
    /// ReflectionGenerator::__construct(): Argument #1 ($generator) must be of type Generator, stdClass given
    /// ```
    fn __construct(req: &mut Request, this: This<Self>, generator: Instance<crate::Generator>) {
        let object = generator.object();
        keep(req, this.object(), object.class(), Some(object));
    }

    /// `ReflectionGenerator::getExecutingLine()`: the line of the
    /// opline the generator's frame stands at, the `yield` it waits at
    /// or the `GENERATOR_CREATE` of a generator not started.
    ///
    /// ```php
    /// function g() {
    ///     yield 1;
    ///     yield 2;
    /// }
    /// $g = g(); $r = new ReflectionGenerator($g);
    /// var_dump($r->getExecutingLine()); $g->current();
    /// var_dump($r->getExecutingLine()); $g->next();
    /// var_dump($r->getExecutingLine()); $g->next();
    /// try { $r->getExecutingLine(); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(2)
    /// int(3)
    /// int(4)
    /// Cannot fetch information from a closed Generator
    /// ```
    fn getExecutingLine(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let (_, execution) = standing_generator(req, this.object())?;
        Ok(execution.site.map_or(0, |(_, line)| i64::from(line)))
    }

    /// `ReflectionGenerator::getExecutingFile()`: the file of the
    /// generator's frame.
    ///
    /// ```php
    /// function g() { yield 1; }
    /// $g = g();
    /// var_dump(basename((new ReflectionGenerator($g))->getExecutingFile()));
    /// ```
    /// ```expect
    /// string(11) "witness.php"
    /// ```
    fn getExecutingFile(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let (_, execution) = standing_generator(req, this.object())?;
        let file = execution.site.map(|(file, _)| file).unwrap_or_default();
        PhpString::new(req, &file)
    }

    /// `ReflectionGenerator::getTrace(int $options =
    /// DEBUG_BACKTRACE_PROVIDE_OBJECT)`: the frames of the delegation, the
    /// generator whose body runs first, down to this one, whose row names
    /// no file.
    ///
    /// ```php
    /// class C { function inner($x) { yield $x; } function outer() { yield from $this->inner(7); } }
    /// $g = (new C)->outer(); $g->current();
    /// $r = new ReflectionGenerator($g);
    /// var_dump($r->getTrace(), $r->getTrace(DEBUG_BACKTRACE_IGNORE_ARGS));
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   array(7) {
    ///     ["file"]=>
    ///     string(%d) "%s"
    ///     ["line"]=>
    ///     int(2)
    ///     ["function"]=>
    ///     string(5) "inner"
    ///     ["class"]=>
    ///     string(1) "C"
    ///     ["object"]=>
    ///     object(C)#1 (0) {
    ///     }
    ///     ["type"]=>
    ///     string(2) "->"
    ///     ["args"]=>
    ///     array(1) {
    ///       [0]=>
    ///       int(7)
    ///     }
    ///   }
    ///   [1]=>
    ///   array(5) {
    ///     ["function"]=>
    ///     string(5) "outer"
    ///     ["class"]=>
    ///     string(1) "C"
    ///     ["object"]=>
    ///     object(C)#1 (0) {
    ///     }
    ///     ["type"]=>
    ///     string(2) "->"
    ///     ["args"]=>
    ///     array(0) {
    ///     }
    ///   }
    /// }
    /// array(2) {
    ///   [0]=>
    ///   array(5) {
    ///     ["file"]=>
    ///     string(%d) "%s"
    ///     ["line"]=>
    ///     int(2)
    ///     ["function"]=>
    ///     string(5) "inner"
    ///     ["class"]=>
    ///     string(1) "C"
    ///     ["type"]=>
    ///     string(2) "->"
    ///   }
    ///   [1]=>
    ///   array(3) {
    ///     ["function"]=>
    ///     string(5) "outer"
    ///     ["class"]=>
    ///     string(1) "C"
    ///     ["type"]=>
    ///     string(2) "->"
    ///   }
    /// }
    /// ```
    fn getTrace(
        req: &mut Request,
        this: This<Self>,
        #[php(default = DEBUG_BACKTRACE_PROVIDE_OBJECT)] options: i64,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let (kept, _) = standing_generator(req, this.object())?;
        trace_of(req, kept, options)
    }

    /// `ReflectionGenerator::getFunction()`: `generator->func`, readable
    /// after the generator closed: a closure's reflected over the closure
    /// as a `ReflectionFunction`, a method's as a `ReflectionMethod` of its
    /// class, and a function's by its name.
    ///
    /// ```php
    /// class C { function m() { yield; } }
    /// function f() { yield; }
    /// foreach ([(new C)->m(), f(), (function () { yield; })->bindTo(new C, C::class)()] as $g) {
    ///     foreach ($g as $_);
    ///     $fn = (new ReflectionGenerator($g))->getFunction();
    ///     echo get_class($fn), ' ', $fn->name, ' ', $fn instanceof ReflectionMethod ? $fn->class : '-', "\n";
    /// }
    /// ```
    /// ```expect
    /// ReflectionMethod m C
    /// ReflectionFunction f -
    /// ReflectionFunction {closure:%s:4} -
    /// ```
    fn getFunction(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<ReflectionFunctionAbstract>>, Thrown> {
        let (_, execution) = kept_execution(req, this.object())?;
        let Some(function) = execution.function else {
            return Err(fatal(req, "InternalError: a generator with no function"));
        };
        let name = req
            .function_decl(function)
            .map(|decl| decl.name.0.to_vec())
            .unwrap_or_default();
        let object = match (execution.closure, execution.scope) {
            (Some(closure), _) => {
                let class = class_named(req, b"ReflectionFunction")?;
                let object = new_object(req, class)?;
                set_name(req, *object, &name)?;
                keep_closure(req, *object, closure);
                object
            }
            (None, Some(scope)) => {
                let held = req.method_row(scope, &name).map(HeldMethod::Row);
                crate::class::method_factory(req, &name, scope, scope, held)?
            }
            (None, None) => {
                let class = class_named(req, b"ReflectionFunction")?;
                let object = new_object(req, class)?;
                set_name(req, *object, &name)?;
                object
            }
        };
        Ok(object.into_instance())
    }

    /// `ReflectionGenerator::getThis()`: `$this` of the generator's
    /// frame, `null` for one outside an object.
    ///
    /// ```php
    /// class C { function m() { yield; } }
    /// function f() { yield; }
    /// var_dump((new ReflectionGenerator((new C)->m()))->getThis(), (new ReflectionGenerator(f()))->getThis());
    /// ```
    /// ```expect
    /// object(C)#2 (0) {
    /// }
    /// NULL
    /// ```
    fn getThis(req: &mut Request, this: This<Self>) -> Result<Option<PhpObject>, Thrown> {
        let (_, execution) = standing_generator(req, this.object())?;
        Ok(execution.this)
    }

    /// `ReflectionGenerator::getExecutingGenerator()`: the generator
    /// whose body a resumption runs, the innermost one a `yield from`
    /// delegates to (`zend_generator_get_current`).
    ///
    /// ```php
    /// function inner() { yield 1; }
    /// function outer() { yield from inner(); }
    /// $g = outer(); $g->current();
    /// var_dump((new ReflectionGenerator($g))->getExecutingGenerator());
    /// ```
    /// ```expect
    /// object(Generator)#2 (1) {
    ///   ["function"]=>
    ///   string(5) "inner"
    /// }
    /// ```
    fn getExecutingGenerator(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Instance<crate::Generator>, Thrown> {
        let (kept, execution) = standing_generator(req, this.object())?;
        let executing = execution.executing.unwrap_or(kept);
        Ok(Instance::of(executing))
    }

    /// `ReflectionGenerator::isClosed()`: `execute_data == NULL`, the
    /// generator returned, threw, or was never going to yield.
    ///
    /// ```php
    /// function g() { yield 1; }
    /// $g = g(); $r = new ReflectionGenerator($g);
    /// var_dump($r->isClosed()); foreach ($g as $_); var_dump($r->isClosed());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// ```
    fn isClosed(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, execution) = kept_execution(req, this.object())?;
        Ok(execution.executing.is_none())
    }
}

/// `ReflectionMethod` as the stub declares it.
#[php_class(extends(ReflectionFunctionAbstract), size = 96, payload = crate::Reflected, handlers = crate::HANDLERS)]
pub(crate) struct ReflectionMethod;

#[php_class]
impl ReflectionMethod {
    const IS_STATIC: i64 = 16;
    const IS_PUBLIC: i64 = 1;
    const IS_PROTECTED: i64 = 2;
    const IS_PRIVATE: i64 = 4;
    const IS_ABSTRACT: i64 = 64;
    const IS_FINAL: i64 = 32;
    #[php(property)]
    const class: PhpString;

    /// `instantiate_reflection_method` as the constructor,
    /// `ReflectionMethod::__construct(object|string $objectOrMethod,
    /// ?string $method = null)`: an object and a name, a class name and a
    /// name, or one `Class::method` string, which 8.5 deprecates in favour
    /// of `createFromMethodName`; the class is `zend_lookup_class` or the
    /// `ReflectionException` every reflector throws, the method is the
    /// linked table's entry under the lowered name or `Method C::m() does
    /// not exist`. `$name` is the method's declared spelling and `$class`
    /// its scope's name, `Exception` for a method `JsonException` inherits,
    /// as `mptr->common.scope->name` is. php keeps the class it was asked
    /// about too, `intern->ce`, which `__toString` renders against; the
    /// reflector here keeps the two properties alone until a native
    /// payload can hold it (#360), so it renders against the declaring
    /// class.
    ///
    /// ```php
    /// $m = new ReflectionMethod('JsonException', 'getcode'); $n = new ReflectionMethod(new JsonException, 'getMessage'); var_dump($m->name, $m->class, $n->name, $n->class);
    /// ```
    /// ```expect
    /// string(7) "getCode"
    /// string(9) "Exception"
    /// string(10) "getMessage"
    /// string(9) "Exception"
    /// ```
    ///
    /// ```php
    /// new ReflectionMethod('Exception::nope');
    /// ```
    /// ```expect exit=255
    /// Deprecated: Calling ReflectionMethod::__construct() with 1 argument is deprecated, use ReflectionMethod::createFromMethodName() instead in %s on line 2
    ///
    /// Fatal error: Uncaught ReflectionException: Method Exception::nope() does not exist in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionMethod->__construct('Exception::nope')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// new ReflectionMethod('nocolons');
    /// ```
    /// ```expect exit=255
    /// Deprecated: Calling ReflectionMethod::__construct() with 1 argument is deprecated, use ReflectionMethod::createFromMethodName() instead in %s on line 2
    ///
    /// Fatal error: Uncaught ReflectionException: ReflectionMethod::__construct(): Argument #1 ($objectOrMethod) must be a valid method name in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionMethod->__construct('nocolons')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// new ReflectionMethod(new Exception);
    /// ```
    /// ```expect exit=255
    /// Deprecated: Calling ReflectionMethod::__construct() with 1 argument is deprecated, use ReflectionMethod::createFromMethodName() instead in %s on line 2
    ///
    /// Fatal error: Uncaught ValueError: ReflectionMethod::__construct(): Argument #2 ($method) cannot be null when argument #1 ($objectOrMethod) is an object in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionMethod->__construct(Object(Exception))
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// try { new ReflectionMethod('Exception', []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// ReflectionMethod::__construct(): Argument #2 ($method) must be of type ?string, array given
    /// ```
    ///
    /// ```php
    /// new ReflectionMethod();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: ReflectionMethod::__construct() expects at least 1 argument, 0 given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionMethod->__construct()
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// A name with no `::` is `zend_argument_error`'s, so the method's
    /// member string and the two printed buffers stand across the throw
    /// and it peaks where php's does; the deprecation is silenced, as
    /// what its display lays down is not this throw's:
    ///
    /// ```php
    /// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
    /// pk(function () { try { @new ReflectionMethod('a'); } catch (ReflectionException $e) {} });
    /// pk(function () { try { ReflectionMethod::createFromMethodName('a'); } catch (ReflectionException $e) {} });
    /// ```
    /// ```expect
    /// 3120 32
    /// 3016 0
    /// ```
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        objectOrMethod: Raw<Union<PhpObject, PhpString>>,
        #[php(default = null)] method: Raw<Option<PhpString>>,
    ) -> Result<(), Thrown> {
        let count = req.argument_count();
        if count == 0 {
            return Err(arg_count_error(req, "at least 1 argument, 0 given"));
        }
        at_most_arguments(req, 2)?;
        if count == 1 {
            deprecated(
                req,
                "Calling ReflectionMethod::__construct() with 1 argument is deprecated, use ReflectionMethod::createFromMethodName() instead",
            );
        }
        // `Z_PARAM_OBJ_OR_STR` then `Z_PARAM_STR_OR_NULL`, both parsed
        // before either is looked at.
        let first = objectOrMethod.hold(req)?;
        let second = method.hold(req)?.map(|name| name.as_bytes().to_vec());
        let (class, method_name) = match (first, second) {
            (Union::A(object), Some(name)) => (object.class(), name),
            (Union::A(_), None) => {
                return Err(argument_error(
                    req,
                    "ValueError",
                    2,
                    "cannot be null when argument #1 ($objectOrMethod) is an object",
                ));
            }
            (Union::B(class), Some(name)) => (class_named(req, class.as_bytes())?, name),
            (Union::B(given), None) => {
                let given = given.as_bytes();
                let Some(at) = given.windows(2).position(|pair| pair == b"::") else {
                    return Err(argument_error(
                        req,
                        "ReflectionException",
                        1,
                        "must be a valid method name",
                    ));
                };
                let class = class_named(req, &given[..at])?;
                (class, given[at + 2..].to_vec())
            }
        };
        // `zend_get_closure_invoke_method(orig_obj)` for a closure's
        // `__invoke`, which php reaches only with the closure in hand.
        let closure = match first {
            Union::A(object) => Some(object).filter(|o| o.closure(req).is_some()),
            Union::B(_) => None,
        };
        let found = match (closure, req.method_row(class, &method_name)) {
            (_, Some(held)) => super::LinkedMethod::of_row(req, held),
            (Some(closure), None) => method_or_invoke(req, class, &method_name, Some(closure)),
            (None, None) => None,
        };
        let Some(found) = found else {
            let class = class_name_of(req, class);
            return Err(throw(
                req,
                "ReflectionException",
                &format!(
                    "Method {}::{}() does not exist",
                    String::from_utf8_lossy(&class),
                    String::from_utf8_lossy(&method_name)
                ),
            ));
        };
        let scope = class_name_of(req, found.scope);
        let this = this.object();
        set_name(req, this, &found.decl.function.name.0)?;
        set_string_prop(req, this, b"class", &scope)?;
        // `intern->ce = ce`: the class the reflector was asked about,
        // which `_function_string` takes as the method's scope, where
        // `$class` is the class that declared the method. A closure's
        // `__invoke` leaves `intern->obj` unset, the trampoline holding
        // the closure's function and no owner of the closure.
        keep(req, this, class, None);
        keep_method(req, this, found.held);
        Ok(())
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
    fn createFromMethodName(req: &mut Request, method: PhpString) -> Result<Owner<Static>, Thrown> {
        let given = method.as_bytes();
        let Some(at) = given.windows(2).position(|pair| pair == b"::") else {
            return Err(argument_error(
                req,
                "ReflectionException",
                1,
                "must be a valid method name",
            ));
        };
        let class = class_named(req, &given[..at])?;
        let name = given[at + 2..].to_vec();
        let held = req.method_row(class, &name).map(HeldMethod::Row);
        let Some(found) = held.and_then(|held| super::LinkedMethod::of_held(req, held)) else {
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
        set_name(req, *object, &found.decl.function.name.0)?;
        set_string_prop(req, *object, b"class", &scope)?;
        super::keep(req, *object, class, None);
        keep_method(req, *object, held);
        Ok(object.into_static())
    }

    /// `ReflectionMethod::__toString()`: `_function_string` over the
    /// method with `intern->ce`, the class the reflector was asked about,
    /// as its scope, indent empty. The
    /// tentative bit renders as `Tentative return`, a declared return
    /// type as `Return`, and a method with neither has no row.
    ///
    /// ```php
    /// echo new ReflectionMethod('JsonException', 'getMessage');
    /// ```
    /// ```expect
    /// Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getMessage ] {
    ///
    ///   - Parameters [0] {
    ///   }
    ///   - Return [ string ]
    /// }
    /// ```
    ///
    /// ```php
    /// echo new ReflectionMethod('JsonSerializable::jsonSerialize');
    /// ```
    /// ```expect
    /// Deprecated: Calling ReflectionMethod::__construct() with 1 argument is deprecated, use ReflectionMethod::createFromMethodName() instead in %s on line 2
    /// Method [ <internal:json> abstract public method jsonSerialize ] {
    ///
    ///   - Parameters [0] {
    ///   }
    ///   - Tentative return [ mixed ]
    /// }
    /// ```
    ///
    /// ```php
    /// echo new ReflectionMethod('Exception', 'getMessage'), new ReflectionMethod('Exception', 'getCode'), new ReflectionMethod('Exception', '__construct');
    /// ```
    /// ```expect
    /// Method [ <internal:Core, prototype Throwable> final public method getMessage ] {
    ///
    ///   - Parameters [0] {
    ///   }
    ///   - Return [ string ]
    /// }
    /// Method [ <internal:Core, prototype Throwable> final public method getCode ] {
    ///
    ///   - Parameters [0] {
    ///   }
    /// }
    /// Method [ <internal:Core, ctor> public method __construct ] {
    ///
    ///   - Parameters [3] {
    ///     Parameter #0 [ <optional> string $message = "" ]
    ///     Parameter #1 [ <optional> int $code = 0 ]
    ///     Parameter #2 [ <optional> ?Throwable $previous = null ]
    ///   }
    /// }
    /// ```
    ///
    /// A hook's reflector renders the hook it holds, which no method
    /// table has:
    ///
    /// ```php
    /// class A { public string $p { get => "x"; set => strtoupper($value); } }
    /// echo (new ReflectionProperty("A", "p"))->getHook(PropertyHookType::Get);
    /// echo (new ReflectionProperty("A", "p"))->getHook(PropertyHookType::Set);
    /// ```
    /// ```expect
    /// Method [ <user> public method $p::get ] {
    ///   @@ %s 2 - 2
    ///
    ///   - Parameters [0] {
    ///   }
    ///   - Return [ string ]
    /// }
    /// Method [ <user> public method $p::set ] {
    ///   @@ %s 2 - 2
    ///
    ///   - Parameters [1] {
    ///     Parameter #0 [ <required> string $value ]
    ///   }
    ///   - Return [ void ]
    /// }
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let name = this_name(req, this.object(), "ReflectionMethod")?;
        let Some(class) = this
            .object()
            .prop(req, b"class")
            .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
        else {
            return Err(throw_error(
                req,
                "Error",
                "Typed property ReflectionMethod::$class must not be accessed before initialization",
            ));
        };
        let class = class_named(req, &class)?;
        let hook = kept_hook_method(req, this.object(), class, &name).map(|(scope, decl)| {
            super::LinkedMethod {
                decl,
                scope,
                held: None,
            }
        });
        let held = kept_method(req, this.object());
        let kept = kept_closure(req, this.object());
        let Some(found) = hook
            .or_else(|| super::LinkedMethod::of_held(req, held?))
            .or_else(|| method_or_invoke(req, class, &name, kept))
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
        // `_function_string(&str, mptr, intern->ce, "")`: the scope is the
        // class the reflector was asked about, so a method the class
        // inherited carries `inherits` and the declaring class's own
        // carries `overwrites`.
        let scope = kept_class(req, this.object()).unwrap_or(class);
        let text = method_string(req, &found, scope, "");
        PhpString::new(req, text.as_bytes())
    }

    /// `ReflectionMethod::isPublic()`: `ZEND_ACC_PUBLIC`.
    ///
    /// ```php
    /// var_dump((new ReflectionMethod('Exception', 'getCode'))->isPublic());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isPublic(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, decl, _) = this_method(req, this.object())?;
        Ok(decl.visibility == Visibility::Public)
    }

    /// `ReflectionMethod::isPrivate()`: `ZEND_ACC_PRIVATE`.
    ///
    /// ```php
    /// var_dump((new ReflectionMethod('Exception', '__clone'))->isPrivate());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isPrivate(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, decl, _) = this_method(req, this.object())?;
        Ok(decl.visibility == Visibility::Private)
    }

    /// `ReflectionMethod::isProtected()`: `ZEND_ACC_PROTECTED`.
    ///
    /// ```php
    /// var_dump((new ReflectionMethod('Exception', 'getCode'))->isProtected());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isProtected(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, decl, _) = this_method(req, this.object())?;
        Ok(decl.visibility == Visibility::Protected)
    }

    /// `ReflectionMethod::isAbstract()`: `ZEND_ACC_ABSTRACT`.
    ///
    /// ```php
    /// var_dump((new ReflectionMethod('JsonSerializable', 'jsonSerialize'))->isAbstract());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isAbstract(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, decl, _) = this_method(req, this.object())?;
        Ok(decl.is_abstract)
    }

    /// `ReflectionMethod::isFinal()`: `ZEND_ACC_FINAL`.
    ///
    /// ```php
    /// var_dump((new ReflectionMethod('Exception', 'getCode'))->isFinal());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isFinal(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, decl, _) = this_method(req, this.object())?;
        Ok(decl.is_final)
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
    #[php(tentative)]
    fn isConstructor(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, decl, _) = this_method(req, this.object())?;
        Ok(is_constructor(&decl))
    }

    /// `ReflectionMethod::isDestructor()`: the method named `__destruct`.
    ///
    /// ```php
    /// var_dump((new ReflectionMethod('Exception', '__construct'))->isDestructor());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isDestructor(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, decl, _) = this_method(req, this.object())?;
        Ok(decl.function.name.0.eq_ignore_ascii_case(b"__destruct"))
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
    #[php(tentative)]
    fn getClosure(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] object: Option<PhpObject>,
    ) -> Result<Owner<Instance<crate::Closure>>, Thrown> {
        let (_, decl, scope) = this_method(req, this.object())?;
        let name = decl.function.name.clone();
        if !decl.is_instance {
            return fake_closure(req, scope, &name, None);
        }
        let Some(object) = object else {
            return Err(argument_error(
                req,
                "ValueError",
                1,
                "cannot be null for non-static methods",
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
            return Ok(Owner::take(req, object).into_instance());
        }
        fake_closure(req, scope, &name, Some(object))
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
    #[php(tentative)]
    fn getModifiers(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let (_, decl, _) = this_method(req, this.object())?;
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
        Ok(bits)
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
    #[php(tentative, arity = manual)]
    fn invoke(
        req: &mut Request,
        this: This<Self>,
        object: Raw<Option<PhpObject>>,
        args: NamedVariadic<Value>,
    ) -> Result<Owner<Value>, Thrown> {
        let (class, decl, scope) = this_method(req, this.object())?;
        let class = super::kept_class(req, this.object()).unwrap_or(class);
        abstract_refused(req, &decl, scope)?;
        at_least_arguments(req, 1)?;
        let this = receiver(req, &decl, scope, object)?;
        let named = args.named().to_vec();
        let arguments = args.positional;
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
    #[php(tentative, arity = manual)]
    fn invokeArgs(
        req: &mut Request,
        this: This<Self>,
        object: Raw<Option<PhpObject>>,
        args: Raw<PhpArray>,
    ) -> Result<Owner<Value>, Thrown> {
        let (class, decl, scope) = this_method(req, this.object())?;
        let class = super::kept_class(req, this.object()).unwrap_or(class);
        abstract_refused(req, &decl, scope)?;
        exactly_arguments(req, 2)?;
        let this = receiver(req, &decl, scope, object)?;
        let array = args.hold(req)?;
        invoke_method(req, class, &decl, scope, this, |req, callable| {
            call_array(req, callable, array)
        })
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
    #[php(tentative)]
    fn getDeclaringClass(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<crate::class::ReflectionClass>>, Thrown> {
        let (_, _, scope) = this_method(req, this.object())?;
        let object = reflection_class_factory(req, scope)?;
        Ok(object.into_instance())
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
    #[php(tentative)]
    fn getPrototype(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<ReflectionMethod>>, Thrown> {
        let name = this_name(req, this.object(), "ReflectionMethod")?;
        let Some(prototype) = prototype_of(req, this.object(), &name)? else {
            let (class, _, _) = this_method(req, this.object())?;
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
        set_name(req, *object, &name)?;
        set_string_prop(req, *object, b"class", &scope)?;
        Ok(object.into_instance())
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
    fn hasPrototype(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let name = this_name(req, this.object(), "ReflectionMethod")?;
        Ok(prototype_of(req, this.object(), &name)?.is_some())
    }

    /// `ReflectionMethod::setAccessible(bool $accessible)`: the argument
    /// parsed and nothing done, every method being invocable through
    /// Reflection since 8.1. `ReflectionProperty::setAccessible` shares
    /// the body.
    ///
    /// ```php
    /// class A { private function f() { return 'f'; } private $p = 'p'; }
    /// $m = new ReflectionMethod('A', 'f'); var_dump($m->setAccessible(false), $m->invoke(new A));
    /// $p = new ReflectionProperty('A', 'p'); $p->setAccessible(false); var_dump($p->getValue(new A));
    /// ```
    /// ```expect
    /// Deprecated: Method ReflectionMethod::setAccessible() is deprecated since 8.5, as it has no effect since PHP 8.1 in %s on line 3
    /// NULL
    /// string(1) "f"
    ///
    /// Deprecated: Method ReflectionProperty::setAccessible() is deprecated since 8.5, as it has no effect since PHP 8.1 in %s on line 4
    /// string(1) "p"
    /// ```
    #[php(
        tentative,
        deprecated(since = "8.5", message = "as it has no effect since PHP 8.1")
    )]
    fn setAccessible(_: This<Self>, accessible: bool) {
        let _ = accessible;
    }
}

/// `ReflectionFiber` as the stub declares it.
#[php_class(final, not_serializable, size = 96, payload = crate::Reflected, handlers = crate::HANDLERS)]
pub(crate) struct ReflectionFiber;

#[php_class]
impl ReflectionFiber {
    /// `ReflectionFiber::__construct(Fiber $fiber)`: the fiber kept,
    /// whatever its state.
    ///
    /// ```php
    /// var_dump(new ReflectionFiber(new Fiber(fn () => 1)));
    /// try { new ReflectionFiber(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(ReflectionFiber)#1 (0) {
    /// }
    /// ReflectionFiber::__construct(): Argument #1 ($fiber) must be of type Fiber, stdClass given
    /// ```
    fn __construct(req: &mut Request, this: This<Self>, fiber: Instance<crate::Fiber>) {
        let object = fiber.object();
        keep(req, this.object(), object.class(), Some(object));
    }

    /// `ReflectionFiber::getFiber()`: the fiber kept.
    ///
    /// ```php
    /// $f = new Fiber(fn () => 1);
    /// var_dump((new ReflectionFiber($f))->getFiber() === $f);
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    fn getFiber(req: &mut Request, this: This<Self>) -> Result<Instance<crate::Fiber>, Thrown> {
        let (kept, _) = kept_execution(req, this.object())?;
        Ok(Instance::of(kept))
    }

    /// `ReflectionFiber::getExecutingFile()`: the file of the innermost
    /// frame of user code on the fiber's stack, the caller's own while
    /// the fiber runs, and `null` where the stack holds only natives.
    ///
    /// ```php
    /// $f = new Fiber(function () { echo basename((new ReflectionFiber(Fiber::getCurrent()))->getExecutingFile()), "\n"; Fiber::suspend(); });
    /// try { (new ReflectionFiber($f))->getExecutingFile(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $f->start();
    /// var_dump(basename((new ReflectionFiber($f))->getExecutingFile()));
    /// $g = new Fiber(Fiber::suspend(...)); $g->start();
    /// var_dump((new ReflectionFiber($g))->getExecutingFile());
    /// ```
    /// ```expect
    /// Cannot fetch information from a fiber that has not been started or is terminated
    /// witness.php
    /// string(11) "witness.php"
    /// NULL
    /// ```
    fn getExecutingFile(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        let (_, execution) = standing_fiber(req, this.object())?;
        match execution.site {
            Some((file, _)) => PhpString::new(req, &file).map(Some),
            None => Ok(None),
        }
    }

    /// `ReflectionFiber::getExecutingLine()`: the line of that frame,
    /// `null` where there is none.
    ///
    /// ```php
    /// function park() {
    ///     Fiber::suspend();
    /// }
    /// $f = new Fiber(function () {
    ///     var_dump((new ReflectionFiber(Fiber::getCurrent()))->getExecutingLine());
    ///     park();
    /// });
    /// $r = new ReflectionFiber($f);
    /// try { $r->getExecutingLine(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $f->start();
    /// var_dump($r->getExecutingLine());
    /// $f->resume();
    /// try { $r->getExecutingLine(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $g = new Fiber(Fiber::suspend(...)); $g->start();
    /// var_dump((new ReflectionFiber($g))->getExecutingLine());
    /// ```
    /// ```expect
    /// Cannot fetch information from a fiber that has not been started or is terminated
    /// int(6)
    /// int(3)
    /// Cannot fetch information from a fiber that has not been started or is terminated
    /// NULL
    /// ```
    fn getExecutingLine(req: &mut Request, this: This<Self>) -> Result<Option<i64>, Thrown> {
        let (_, execution) = standing_fiber(req, this.object())?;
        Ok(execution.site.map(|(_, line)| i64::from(line)))
    }

    /// `ReflectionFiber::getCallable()`: the callable the fiber was made
    /// with, as it was given, until the fiber is dead.
    ///
    /// ```php
    /// $c = fn () => Fiber::suspend();
    /// $f = new Fiber($c); $r = new ReflectionFiber($f);
    /// var_dump($r->getCallable() === $c); $f->start(); var_dump($r->getCallable() === $c);
    /// $f->resume();
    /// try { $r->getCallable(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump((new ReflectionFiber(new Fiber('strlen')))->getCallable());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// Cannot fetch the callable from a fiber that has terminated
    /// string(6) "strlen"
    /// ```
    fn getCallable(req: &mut Request, this: This<Self>) -> Result<Callable, Thrown> {
        let (_, execution) = kept_execution(req, this.object())?;
        execution.callable.ok_or_else(|| {
            throw_error(
                req,
                "Error",
                "Cannot fetch the callable from a fiber that has terminated",
            )
        })
    }

    /// `ReflectionFiber::getTrace(int $options =
    /// DEBUG_BACKTRACE_PROVIDE_OBJECT)`: every link on the fiber's stack,
    /// innermost first, the bottom one naming no file.
    ///
    /// ```php
    /// class T { function __invoke($a) { self::park($a); } static function park($a) { Fiber::suspend(); } }
    /// $f = new Fiber(new T); $f->start('x');
    /// var_dump((new ReflectionFiber($f))->getTrace());
    /// ```
    /// ```expect
    /// array(3) {
    ///   [0]=>
    ///   array(6) {
    ///     ["file"]=>
    ///     string(%d) "%s"
    ///     ["line"]=>
    ///     int(2)
    ///     ["function"]=>
    ///     string(7) "suspend"
    ///     ["class"]=>
    ///     string(5) "Fiber"
    ///     ["type"]=>
    ///     string(2) "::"
    ///     ["args"]=>
    ///     array(0) {
    ///     }
    ///   }
    ///   [1]=>
    ///   array(6) {
    ///     ["file"]=>
    ///     string(%d) "%s"
    ///     ["line"]=>
    ///     int(2)
    ///     ["function"]=>
    ///     string(4) "park"
    ///     ["class"]=>
    ///     string(1) "T"
    ///     ["type"]=>
    ///     string(2) "::"
    ///     ["args"]=>
    ///     array(1) {
    ///       [0]=>
    ///       string(1) "x"
    ///     }
    ///   }
    ///   [2]=>
    ///   array(5) {
    ///     ["function"]=>
    ///     string(8) "__invoke"
    ///     ["class"]=>
    ///     string(1) "T"
    ///     ["object"]=>
    ///     object(T)#2 (0) {
    ///     }
    ///     ["type"]=>
    ///     string(2) "->"
    ///     ["args"]=>
    ///     array(1) {
    ///       [0]=>
    ///       string(1) "x"
    ///     }
    ///   }
    /// }
    /// ```
    fn getTrace(
        req: &mut Request,
        this: This<Self>,
        #[php(default = DEBUG_BACKTRACE_PROVIDE_OBJECT)] options: i64,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let (kept, _) = standing_fiber(req, this.object())?;
        trace_of(req, kept, options)
    }
}

/// `(fn_flags & (ZEND_ACC_CLOSURE | ZEND_ACC_FAKE_CLOSURE)) ==
/// ZEND_ACC_CLOSURE`: a closure the program wrote, and not one made of a
/// named function.
fn is_anonymous(req: &Request, function: &FunctionRef) -> bool {
    match function {
        FunctionRef::Closure(closure) => closure.closure(req).is_some_and(|info| !info.fake),
        _ => false,
    }
}
