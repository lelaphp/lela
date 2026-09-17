//! The classes Zend itself declares (`zend_exceptions.c`,
//! `zend_closures.c`), which every engine has whether or not an
//! extension is bundled: the throwable hierarchy the raisers of
//! `api::raising` name, and `Closure`. Declared here as an extension's
//! classes are declared, each with its handler table beside its methods
//! (design §5.4), so the linker that arrives in week two (design §11)
//! reads one model and not two; until it does, what a declaration gives
//! is the class a throwable knows itself by, which is the name an
//! uncaught fatal prints.
//!
//! `Closure` is the one class here with a native payload,
//! [`ClosureInfo`], and its table is the one that copies a payload:
//! `clone_obj` is `zend_closure_clone`, `free_obj` gives the bound
//! `$this` back, `get_gc` names it, `get_closure` answers the function,
//! and `compare` is `zend_closure_compare`.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::cmp::Ordering;

use php_unit::decl::{
    ClassDecl, ClassFlags, ClassKind, EnumBacking, FunctionDecl, MethodDecl, Name, Parameter,
    PropertyDecl, Type, Visibility,
};

use crate::api::abi::{CloneObj, Entry, NativeClassDecl, ObjectHandlers};
use crate::api::raising::{Thrown, arg_count_error, throw};
use crate::api::request::Request;
use crate::api::values::{ClosureInfo, PhpObject, Value};

/// `default_exception_handlers`: `clone_obj` null, so every throwable
/// is uncloneable (`zend_register_default_exception`), and the rest
/// `Std`'s. A user class extending a throwable is reached through this
/// table, as php's inherits `create_object`.
struct ThrowableHandlers;

impl ObjectHandlers for ThrowableHandlers {
    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }
}

static THROWABLE: ThrowableHandlers = ThrowableHandlers;

/// `closure_handlers` (`Z/Zend/zend_closures.c:730`): the slots
/// `zend_register_closure_ce` overrides over `std_object_handlers`.
struct ClosureHandlers;

impl ObjectHandlers for ClosureHandlers {
    /// `zend_closure_free_storage`: the bound `$this` given back; the
    /// payload's drop is the heap's.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        if let Some(this) = object
            .native_mut::<ClosureInfo>(req)
            .and_then(|closure| closure.this.take())
        {
            req.release(this.as_value());
        }
    }

    /// `zend_closure_clone`: `zend_create_closure` over the same
    /// function, scope and receiver, the receiver one more owner.
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(closure_clone)
    }

    /// `zend_closure_get_gc`: the bound `$this`, where there is one.
    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(this) = object.closure(req).and_then(|closure| closure.this) {
            visit(this.as_value());
        }
    }

    /// `zend_closure_get_closure`: the payload itself.
    fn get_closure(&self, req: &mut Request, object: PhpObject) -> Option<ClosureInfo> {
        object.closure(req).cloned()
    }

    /// `zend_closure_compare`: two closures are equal when they carry
    /// one function, one receiver and one scope, and uncomparable
    /// otherwise.
    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let (Some(a), Some(b)) = (left.as_object().copied(), right.as_object().copied()) else {
            return Ok(None);
        };
        let (Some(x), Some(y)) = (a.closure(req), b.closure(req)) else {
            return Ok(None);
        };
        let same = x.function == y.function && x.this == y.this && x.scope == y.scope;
        Ok(same.then_some(Ordering::Equal))
    }
}

static CLOSURE: ClosureHandlers = ClosureHandlers;

/// `zend_closure_clone`: the members copied and the payload's copy in
/// the new object's slot, with one more owner of the receiver taken
/// first, as `zend_create_closure` takes it.
fn closure_clone(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let Some(closure) = object.closure(req).cloned() else {
        return req.clone_members(object);
    };
    if let Some(this) = closure.this {
        req.addref(&this.as_value());
    }
    req.clone_with_payload(object, closure)
}

/// `Throwable` and the throwables php declares under it, parents first,
/// each with the methods it declares so far beside it: `Exception` and
/// `Error` carry their private `__clone` (`zend_exceptions.stub.php`),
/// the one method of theirs the visibility rule of
/// `zend_std_get_method` has to be held to. The rest of their methods
/// arrive with the linker (design §6.2).
pub(crate) fn zend_classes() -> Vec<NativeClassDecl> {
    vec![
        (
            declared(b"Stringable", ClassKind::Interface, None),
            Vec::new(),
            None,
        ),
        (
            declared(b"Throwable", ClassKind::Interface, None),
            Vec::new(),
            None,
        ),
        (
            declared(b"Exception", ClassKind::Class, None),
            vec![(private_clone(), throwable_clone as Entry)],
            Some(&THROWABLE),
        ),
        (
            declared(b"Error", ClassKind::Class, None),
            vec![(private_clone(), throwable_clone as Entry)],
            Some(&THROWABLE),
        ),
        (
            declared(b"TypeError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
            None,
        ),
        (
            declared(b"ValueError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
            None,
        ),
        (
            declared(b"ArithmeticError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
            None,
        ),
        (
            declared(
                b"DivisionByZeroError",
                ClassKind::Class,
                Some(b"ArithmeticError"),
            ),
            Vec::new(),
            None,
        ),
        (
            declared(b"ArgumentCountError", ClassKind::Class, Some(b"TypeError")),
            Vec::new(),
            None,
        ),
        (
            declared(b"UnhandledMatchError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
            None,
        ),
        (
            declared(b"CompileError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
            None,
        ),
        (
            declared(b"ParseError", ClassKind::Class, Some(b"CompileError")),
            Vec::new(),
            None,
        ),
        (closure_class(), closure_methods(), Some(&CLOSURE)),
        (std_class(), Vec::new(), None),
    ]
}

/// `class stdClass` as `zend_register_standard_class` declares it
/// (`Z/Zend/zend.c`, `zend_standard_class_def`): no parent, no
/// interface, no member, and `#[AllowDynamicProperties]`, which the
/// declaration model has no bit for yet (#326); what `(object)` builds and
/// what `json_decode` and `unserialize` hand back.
///
/// ```php
/// var_dump(new stdClass, new stdClass instanceof stdClass);
/// ```
/// ```expect
/// object(stdClass)#1 (0) {
/// }
/// bool(true)
/// ```
fn std_class() -> ClassDecl {
    ClassDecl {
        interfaces: Vec::new(),
        ..declared(b"stdClass", ClassKind::Class, None)
    }
}

/// `final class Closure` as `zend_register_closure_ce` declares it: the
/// two of its methods this engine serves, `fromCallable` on the class
/// and `__invoke` on an instance. php synthesises `__invoke` in
/// `zend_closure_get_method` rather than declaring it; it is a declared
/// method here until `get_method` can answer a function the table has
/// no entry for.
fn closure_class() -> ClassDecl {
    ClassDecl {
        flags: ClassFlags {
            is_final: true,
            ..ClassFlags::default()
        },
        interfaces: Vec::new(),
        ..declared(b"Closure", ClassKind::Class, None)
    }
}

fn closure_methods() -> Vec<(MethodDecl, Entry)> {
    let method = |function: FunctionDecl, is_instance: bool| MethodDecl {
        function,
        visibility: Visibility::Public,
        is_instance,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    };
    let function = |name: &[u8], parameters: Vec<Parameter>, returns: Type| FunctionDecl {
        name: Name(name.into()),
        parameters,
        return_type: Some(returns),
        returns_by_ref: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        frameless: Vec::new(),
    };
    let parameter = |name: &[u8], ty: Type, variadic: bool| Parameter {
        name: Name(name.into()),
        ty: Some(ty),
        by_ref: false,
        variadic,
        default: None,
        attributes: Vec::new(),
    };
    vec![
        (
            method(
                function(
                    b"fromCallable",
                    vec![parameter(b"callback", Type::Callable, false)],
                    Type::Named(Name(b"Closure".as_slice().into())),
                ),
                false,
            ),
            closure_from_callable as Entry,
        ),
        (
            method(
                function(
                    b"__invoke",
                    vec![parameter(b"args", Type::Mixed, true)],
                    Type::Mixed,
                ),
                true,
            ),
            closure_invoke as Entry,
        ),
    ]
}

/// `Closure::fromCallable(callable $callback): Closure`,
/// `ZEND_METHOD(Closure, fromCallable)` (`Z/Zend/zend_closures.c:396`):
/// a closure handed in is handed back with one more owner; a function's
/// name is `zend_create_fake_closure` over the function, no receiver
/// and no scope, a new `Closure` of the request's heap carrying
/// [`ClosureInfo`] as its payload; a name nothing declares is php's
/// `TypeError` with `zend_is_callable_ex`'s reason, and what is neither
/// a string nor an array its other reason. The `[$object, 'method']`
/// and `'C::m'` shapes arrive with the callable that reads them.
///
/// ```php
/// $f = Closure::fromCallable('strlen'); $g = clone($f);
/// var_dump($g->__invoke('abcd'), $f === $g, $g instanceof Closure, $f->__invoke('xy'), Closure::fromCallable($f) === $f);
/// ```
/// ```expect
/// int(4)
/// bool(false)
/// bool(true)
/// int(2)
/// bool(true)
/// ```
///
/// ```php
/// Closure::fromCallable('nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Failed to create closure from callable: function "nope" not found or invalid function name in %s:2
/// Stack trace:
/// #0 %s(2): Closure::fromCallable('nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn closure_from_callable(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(arg_count_error(
            req,
            &format!("exactly 1 argument, {count} given"),
        ));
    }
    let given = req.argument(1).unwrap_or_default().deref();
    if let Some(object) = given.as_object().copied()
        && object.closure(req).is_some()
    {
        req.addref(&given);
        return Ok(given);
    }
    let Some(name) = given.as_str_bytes() else {
        return Err(throw(
            req,
            "TypeError",
            "Failed to create closure from callable: no array or string given",
        ));
    };
    let Some(function) = req.function_named(name) else {
        let shown = String::from_utf8_lossy(name).into_owned();
        return Err(throw(
            req,
            "TypeError",
            &format!(
                "Failed to create closure from callable: function \"{shown}\" not found or invalid function name"
            ),
        ));
    };
    let Some(class) = req.class_named(b"Closure") else {
        return Err(throw(req, "Error", "Class \"Closure\" not found"));
    };
    let closure = req.new_object_with(
        class,
        ClosureInfo {
            function,
            this: None,
            scope: None,
        },
    )?;
    Ok(closure.as_value())
}

/// `Closure::__invoke(mixed ...$args): mixed`, `ZEND_METHOD(Closure,
/// __invoke)` (`Z/Zend/zend_closures.c:45`): the payload's function
/// served with the frame's arguments, on the payload's receiver where
/// the closure is bound and as a function where it is not
/// (`zend_call_known_fcc`); what it returns is the call's.
fn closure_invoke(req: &mut Request) -> Result<Value, Thrown> {
    let Some(this) = req.this() else {
        return Err(throw(
            req,
            "Error",
            "Closure::__invoke called with no receiver",
        ));
    };
    let Some(closure) = this.closure(req).cloned() else {
        return Err(throw(req, "Error", "Closure object cannot have properties"));
    };
    let arguments: Vec<Value> = (1..=u32::try_from(req.argument_count()).unwrap_or(u32::MAX))
        .filter_map(|position| req.argument(position))
        .collect();
    match (closure.this, closure.scope) {
        (this, Some(scope)) => req.serve_method(closure.function, this, scope, &arguments),
        (_, None) => req.serve_function(closure.function, &arguments),
    }
}

/// `private function __clone(): void {}`, as `Exception` and `Error`
/// declare it.
fn private_clone() -> MethodDecl {
    MethodDecl {
        function: FunctionDecl {
            name: Name(b"__clone".as_slice().into()),
            parameters: Vec::new(),
            return_type: Some(php_unit::decl::Type::Void),
            returns_by_ref: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            frameless: Vec::new(),
        },
        visibility: Visibility::Private,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }
}

/// `ZEND_METHOD(Exception, __clone)`: private, so a call from outside
/// the class never reaches it; what reflection could reach is php's
/// `Exception` saying an object is not cloned this way.
fn throwable_clone(req: &mut Request) -> Result<Value, Thrown> {
    Err(crate::api::raising::throw(
        req,
        "Exception",
        "Cannot clone object using __clone()",
    ))
}

fn declared(name: &[u8], kind: ClassKind, parent: Option<&[u8]>) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind,
        flags: ClassFlags::default(),
        parent: parent.map(|p| Name(p.into())),
        // php declares the interfaces on `Exception` and `Error` alone
        // and the rest inherit them (`zend_register_default_classes`);
        // the linker is what carries them down (design §6.2).
        interfaces: match (kind, parent) {
            (ClassKind::Class, None) => vec![
                Name(b"Throwable".as_slice().into()),
                Name(b"Stringable".as_slice().into()),
            ],
            _ => Vec::new(),
        },
        traits: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        // php declares the seven properties on `Exception` and `Error`
        // alike (`zend_exceptions.stub.php`), and a child inherits them.
        properties: match (kind, parent) {
            (ClassKind::Class, None) => throwable_properties(),
            _ => Vec::new(),
        },
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    }
}

/// The properties `Exception` and `Error` declare, as
/// `zend_exceptions.stub.php` writes them and in its order: `message`,
/// `string`, `code`, `file`, `line`, `trace`, `previous`. What
/// `zend_default_exception_new` fills at creation, `file`, `line` and
/// `trace`, the raisers of `api::raising` write by name; the rest hold
/// the stub's defaults. Their visibility is what `ReflectionClass::
/// hasProperty` and `getDefaultProperties` read: a child sees the four
/// protected ones and not the three private.
///
/// ```php
/// $r = new ReflectionClass('Exception'); var_dump($r->hasProperty('message'), $r->hasProperty('trace'), (new ReflectionClass('TypeError'))->hasProperty('trace'), $r->getDefaultProperties());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// array(7) {
///   ["message"]=>
///   string(0) ""
///   ["string"]=>
///   string(0) ""
///   ["code"]=>
///   int(0)
///   ["file"]=>
///   string(0) ""
///   ["line"]=>
///   int(0)
///   ["trace"]=>
///   array(0) {
///   }
///   ["previous"]=>
///   NULL
/// }
/// ```
fn throwable_properties() -> Vec<PropertyDecl> {
    use php_unit::decl::{ConstExpr, Type};
    let property = |name: &[u8], visibility, ty, default| PropertyDecl {
        name: Name(name.into()),
        visibility,
        is_instance: true,
        is_readonly: false,
        ty,
        default,
        attributes: Vec::new(),
        doc_comment: None,
    };
    let empty = || Some(ConstExpr::Str(Box::new([])));
    vec![
        property(b"message", Visibility::Protected, None, empty()),
        property(b"string", Visibility::Private, Some(Type::String), empty()),
        property(
            b"code",
            Visibility::Protected,
            None,
            Some(ConstExpr::Long(0)),
        ),
        property(b"file", Visibility::Protected, Some(Type::String), empty()),
        property(
            b"line",
            Visibility::Protected,
            Some(Type::Int),
            Some(ConstExpr::Long(0)),
        ),
        property(
            b"trace",
            Visibility::Private,
            Some(Type::Array),
            Some(ConstExpr::Array(Vec::new())),
        ),
        property(
            b"previous",
            Visibility::Private,
            Some(Type::Nullable(Box::new(Type::Named(Name::from(
                "Throwable",
            ))))),
            Some(ConstExpr::Null),
        ),
    ]
}
