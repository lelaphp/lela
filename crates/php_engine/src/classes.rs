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
    ClassDecl, ClassFlags, ClassKind, ConstExpr, DynamicProperties, EnumBacking, FunctionDecl,
    MethodDecl, Name, Parameter, PropertyDecl, Type, Visibility,
};

use crate::api::abi::{CloneObj, Entry, NativeClassDecl, ObjectHandlers};
use crate::api::call::{Function, Target};
use crate::api::coerce::coerce_param;
use crate::api::raising::{Thrown, arg_count_error, arg_type_error, throw, warning};
use crate::api::request::Request;
use crate::api::values::{
    ClassId, ClosureInfo, Key, PhpArray, PhpObject, PhpString, Value, ValueRef,
};

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

/// Whether `handlers` is the throwables' table: php's
/// `zend_default_exception_new`, the `create_object` of every class
/// under `Throwable`, records the site of `new` on the object as it is
/// made, and `Request::new_object` does the same for an object whose
/// class reaches this table.
pub(crate) fn records_throw_site(handlers: &'static dyn ObjectHandlers) -> bool {
    std::ptr::addr_eq(handlers, &raw const THROWABLE)
}

/// `closure_handlers` (`Z/Zend/zend_closures.c:730`): the slots
/// `zend_register_closure_ce` overrides over `std_object_handlers`.
struct ClosureHandlers;

impl ObjectHandlers for ClosureHandlers {
    /// `zend_closure_free_storage`: the bound `$this` given back; the
    /// payload's drop is the heap's.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let bound = object
            .native_mut::<ClosureInfo>(req)
            .map(|closure| std::mem::take(&mut closure.bound))
            .unwrap_or_default();
        for value in bound {
            req.release(value);
        }
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

    /// `zend_closure_get_gc`: the bound `$this` where there is one, and
    /// the variables the closure bound, which php reaches through
    /// `ZEND_MAP_PTR_GET(closure->func.op_array.static_variables_ptr)`.
    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        let Some(closure) = object.closure(req) else {
            return;
        };
        let this = closure.this;
        for bound in closure.bound.clone() {
            visit(bound);
        }
        if let Some(this) = this {
            visit(this.as_value());
        }
    }

    /// `zend_closure_get_closure`: the payload itself.
    fn get_closure(&self, req: &mut Request, object: PhpObject) -> Option<ClosureInfo> {
        object.closure(req).cloned()
    }

    /// `zend_closure_get_method` (`Z/Zend/zend_closures.c:528`):
    /// `__invoke` is the trampoline `zend_get_closure_invoke_method`
    /// makes on the lookup, a function `Closure`'s table has no row
    /// for, and every other name falls to `zend_std_get_method`. So
    /// `$f->__invoke()` reaches the same entry `$f()` reaches, and
    /// nothing that reads the table, a rendering or a reflector, sees
    /// a seventh method on the class.
    ///
    /// ```php
    /// $f = function ($x) { return $x + 1; };
    /// var_dump($f->__invoke(1), $f->__INVOKE(2), $f(3));
    /// class C { public $p = 4; public function m() { return function () { return $this->p; }; } }
    /// $g = (new C)->m();
    /// var_dump($g->__invoke());
    /// ```
    /// ```expect
    /// int(2)
    /// int(3)
    /// int(4)
    /// int(4)
    /// ```
    ///
    /// ```php
    /// $f = function () {};
    /// $f->nope();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Call to undefined method Closure::nope() in %s:3
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 3
    /// ```
    fn get_method(&self, req: &mut Request, object: PhpObject, lowered: &[u8]) -> Option<Function> {
        if lowered.eq_ignore_ascii_case(INVOKE) {
            return req.trampoline_named(object.class(), INVOKE);
        }
        req.method_named(object.class(), lowered)
    }

    /// `zend_closure_get_debug_info` (`Z/Zend/zend_closures.c:602`): the
    /// table every dumper shows for a closure, in php's order. A closure
    /// the program wrote shows `name`, the name the compiler gave it,
    /// `file` and `line`, the op array's `line_start`; one
    /// `zend_create_fake_closure` made shows `function` instead, the
    /// function's own name, prefixed with its scope where it has one.
    /// Then `static`, the variables the closure bound, left out where it
    /// bound none; `this`, its receiver, where it has one; and
    /// `parameter`, one row per declared parameter, `$name` with a `&`
    /// before it for one taken by reference, against `<required>` up to
    /// the first with a default and `<optional>` from there on, the
    /// variadic among them.
    ///
    /// ```php
    /// $f = function ($a, &$b, $c = 1, ...$r) {};
    /// var_dump($f);
    /// ```
    /// ```expect
    /// object(Closure)#1 (4) {
    ///   ["name"]=>
    ///   string(%d) "{closure:%s:2}"
    ///   ["file"]=>
    ///   string(%d) "%s"
    ///   ["line"]=>
    ///   int(2)
    ///   ["parameter"]=>
    ///   array(4) {
    ///     ["$a"]=>
    ///     string(10) "<required>"
    ///     ["&$b"]=>
    ///     string(10) "<required>"
    ///     ["$c"]=>
    ///     string(10) "<optional>"
    ///     ["$r"]=>
    ///     string(10) "<optional>"
    ///   }
    /// }
    /// ```
    ///
    /// ```php
    /// class C { public $p = 1; public function m() { $n = 1; return function () use ($n) { return $this; }; } }
    /// print_r((new C)->m());
    /// ```
    /// ```expect
    /// Closure Object
    /// (
    ///     [name] => {closure:C::m():2}
    ///     [file] => %s
    ///     [line] => 2
    ///     [static] => Array
    ///         (
    ///             [n] => 1
    ///         )
    ///
    ///     [this] => C Object
    ///         (
    ///             [p] => 1
    ///         )
    ///
    /// )
    /// ```
    ///
    /// ```php
    /// var_dump(Closure::fromCallable('strlen'));
    /// ```
    /// ```expect
    /// object(Closure)#1 (2) {
    ///   ["function"]=>
    ///   string(6) "strlen"
    ///   ["parameter"]=>
    ///   array(1) {
    ///     ["$string"]=>
    ///     string(10) "<required>"
    ///   }
    /// }
    /// ```
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<PhpArray>, Thrown> {
        let Some(closure) = object.closure(req).cloned() else {
            return Ok(None);
        };
        let Some(decl) = req.function_decl(closure.function).cloned() else {
            return PhpArray::new(req).map(Some);
        };
        let mut out = PhpArray::new(req)?;
        let written = String::from_utf8_lossy(&decl.name.0).into_owned();
        if closure.fake {
            let named = match closure.scope {
                Some(scope) => format!("{}::{written}", req.class_name(scope)),
                None => written,
            };
            let named = Value::string(PhpString::new(req, named.as_bytes())?);
            set_row(req, &mut out, b"function", named)?;
        } else {
            let named = Value::string(PhpString::new(req, written.as_bytes())?);
            set_row(req, &mut out, b"name", named)?;
            let file = Value::string(PhpString::new(req, &decl.file.0)?);
            set_row(req, &mut out, b"file", file)?;
            set_row(req, &mut out, b"line", Value::int(i64::from(decl.line)))?;
        }
        let bound = object.closure_bound(req)?;
        if !bound.is_empty() {
            let mut statics = PhpArray::new(req)?;
            for (name, value) in bound {
                req.addref(&value);
                statics.set(req, Key::from_string(name), value)?;
            }
            set_row(req, &mut out, b"static", Value::array(statics))?;
        }
        if let Some(this) = closure.this {
            let this = this.as_value();
            req.addref(&this);
            set_row(req, &mut out, b"this", this)?;
        }
        if !decl.parameters.is_empty() {
            let required = decl
                .parameters
                .iter()
                .position(|p| p.default.is_some() || p.variadic)
                .unwrap_or(decl.parameters.len());
            let mut parameters = PhpArray::new(req)?;
            for (at, parameter) in decl.parameters.iter().enumerate() {
                let amp = if parameter.by_ref { "&" } else { "" };
                let name = format!("{amp}${}", String::from_utf8_lossy(&parameter.name.0));
                let stands = if at >= required {
                    &b"<optional>"[..]
                } else {
                    b"<required>"
                };
                let stands = Value::string(PhpString::new(req, stands)?);
                let key = Key::from_string(PhpString::new(req, name.as_bytes())?);
                parameters.set(req, key, stands)?;
            }
            set_row(req, &mut out, b"parameter", Value::array(parameters))?;
        }
        Ok(Some(out))
    }

    /// `zend_closure_compare` (`Z/Zend/zend_closures.c:120`): two fake
    /// closures are equal when they carry one function, one receiver
    /// and one scope; any other pair is uncomparable, a closure the
    /// program declared included, so two of them are equal only where
    /// `zend_compare` found one object on both sides before the
    /// handler was reached. A side that is no closure is
    /// `ZEND_COMPARE_OBJECTS_FALLBACK`, `zend_std_compare_objects`.
    ///
    /// ```php
    /// $f = function () { return 1; };
    /// $g = function () { return 1; };
    /// $h = $f;
    /// var_dump($f == $f, $f == $g, $f == $h, $f === $h, $f == clone $f);
    /// $s = Closure::fromCallable('strlen');
    /// $t = Closure::fromCallable('strlen');
    /// var_dump($s == $t, $s == $f);
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// ```
    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let (Some(a), Some(b)) = (left.as_object().copied(), right.as_object().copied()) else {
            return req.compare_objects(left, right);
        };
        let (Some(x), Some(y)) = (a.closure(req), b.closure(req)) else {
            return req.compare_objects(left, right);
        };
        if !(x.fake && y.fake) {
            return Ok(None);
        }
        let same = x.function == y.function && x.this == y.this && x.scope == y.scope;
        Ok(same.then_some(Ordering::Equal))
    }
}

/// `zend_hash_update(debug_info, ZSTR_KNOWN(...), &val)`: one row of a
/// closure's table, its key a string of the request's that the array
/// takes and its value the array's from here.
fn set_row(
    req: &mut Request,
    table: &mut PhpArray,
    name: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    let key = Key::from_string(PhpString::new(req, name)?);
    table.set(req, key, value)
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
    // `zend_array_dup` of the bound variables in
    // `zend_create_closure_ex`: the clone's table is its own and each
    // value in it one more owner, a reference shared with the original
    // as php shares one.
    for bound in &closure.bound {
        req.addref(bound);
    }
    req.clone_with_payload(object, closure)
}

/// `Throwable` and the throwables php declares under it, parents first,
/// each with its methods beside it as `zend_exceptions.stub.php` writes
/// them: `Stringable` and `Throwable` carry their abstract methods on
/// the declaration, with no entry to pair, and `Exception` and `Error`
/// carry the eleven the stub gives each, `Error`'s being
/// `@implementation-alias`es of `Exception`'s, so one entry serves
/// both. `__clone` is private, the one method the visibility rule of
/// `zend_std_get_method` has to be held to.
pub(crate) fn zend_classes() -> Vec<NativeClassDecl> {
    let mut classes: Vec<NativeClassDecl> = vec![
        (
            ClassDecl {
                methods: vec![abstract_method(b"__toString", Some(Type::String))],
                ..declared(b"Stringable", ClassKind::Interface, None)
            },
            Vec::new(),
            None,
            None,
        ),
        (
            ClassDecl {
                methods: throwable_abstract_methods(),
                ..declared(b"Throwable", ClassKind::Interface, None)
            },
            Vec::new(),
            None,
            None,
        ),
        (
            ClassDecl {
                methods: array_access_methods(),
                ..declared(b"ArrayAccess", ClassKind::Interface, None)
            },
            Vec::new(),
            None,
            None,
        ),
        (
            declared(b"Exception", ClassKind::Class, None),
            throwable_methods(),
            Some(&THROWABLE),
            None,
        ),
        (
            declared(b"Error", ClassKind::Class, None),
            throwable_methods(),
            Some(&THROWABLE),
            None,
        ),
        (
            declared(b"TypeError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
            None,
            None,
        ),
        (
            declared(b"ValueError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
            None,
            None,
        ),
        (
            declared(b"ArithmeticError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
            None,
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
            None,
        ),
        (
            declared(b"ArgumentCountError", ClassKind::Class, Some(b"TypeError")),
            Vec::new(),
            None,
            None,
        ),
        (
            declared(b"UnhandledMatchError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
            None,
            None,
        ),
        (
            declared(b"CompileError", ClassKind::Class, Some(b"Error")),
            Vec::new(),
            None,
            None,
        ),
        (
            declared(b"ParseError", ClassKind::Class, Some(b"CompileError")),
            Vec::new(),
            None,
            None,
        ),
        (closure_class(), closure_methods(), Some(&CLOSURE), None),
        (std_class(), Vec::new(), None, None),
        (unit_enum(), Vec::new(), None, None),
        (backed_enum(), Vec::new(), None, None),
    ];
    classes.push((countable(), Vec::new(), None, None));
    classes.extend(crate::weakrefs::weakref_classes());
    classes
}

/// `interface Countable` as `zend_interfaces.stub.php` declares it:
/// `count(): int`, abstract, its return type tentative. What `count()`
/// asks of an object whose class has no `count_elements`, and what
/// `WeakMap` implements.
///
/// ```php
/// class N implements Countable { function count(): int { return 3; } }
/// var_dump(count(new N), new N instanceof Countable);
/// ```
/// ```expect
/// int(3)
/// bool(true)
/// ```
fn countable() -> ClassDecl {
    ClassDecl {
        methods: vec![MethodDecl {
            tentative_return: true,
            ..abstract_method(b"count", Some(Type::Int))
        }],
        ..declared(b"Countable", ClassKind::Interface, None)
    }
}

/// `interface UnitEnum` as `zend_enum.stub.php` declares it and
/// `zend_register_enum_ce` registers it: `public static function
/// cases(): array;`, abstract as every interface method is. What every
/// enum implements and what gives its `cases` its prototype. The engine
/// serves no enum value yet; the interface is declared so a native enum
/// links and renders, `PropertyHookType` being the first.
///
/// ```php
/// echo new ReflectionClass('UnitEnum');
/// ```
/// ```expect
/// Interface [ <internal:Core> interface UnitEnum ] {
///
///   - Constants [0] {
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [1] {
///     Method [ <internal:Core> abstract static public method cases ] {
///
///       - Parameters [0] {
///       }
///       - Return [ array ]
///     }
///   }
///
///   - Properties [0] {
///   }
///
///   - Methods [0] {
///   }
/// }
/// ```
fn unit_enum() -> ClassDecl {
    ClassDecl {
        methods: vec![abstract_static_method(b"cases", &[], Some(Type::Array))],
        ..declared(b"UnitEnum", ClassKind::Interface, None)
    }
}

/// `interface BackedEnum extends UnitEnum`: `from` and `tryFrom`, each
/// over `int|string $value`, one returning the called class and the
/// other it or null, then `cases` inherited, which is the order
/// `php --re` prints its table in.
///
/// ```php
/// echo new ReflectionClass('BackedEnum');
/// ```
/// ```expect
/// Interface [ <internal:Core> interface BackedEnum extends UnitEnum ] {
///
///   - Constants [0] {
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [3] {
///     Method [ <internal:Core> abstract static public method from ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> string|int $value ]
///       }
///       - Return [ static ]
///     }
///
///     Method [ <internal:Core> abstract static public method tryFrom ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> string|int $value ]
///       }
///       - Return [ ?static ]
///     }
///
///     Method [ <internal:Core, inherits UnitEnum> abstract static public method cases ] {
///
///       - Parameters [0] {
///       }
///       - Return [ array ]
///     }
///   }
///
///   - Properties [0] {
///   }
///
///   - Methods [0] {
///   }
/// }
/// ```
fn backed_enum() -> ClassDecl {
    let value = || Parameter {
        name: Name::from("value"),
        ty: Some(Type::Union(vec![Type::Int, Type::String])),
        by_ref: false,
        prefer_ref: false,
        variadic: false,
        default: None,
        attributes: Vec::new(),
    };
    ClassDecl {
        interfaces: vec![Name::from("UnitEnum")],
        methods: vec![
            abstract_static_method(b"from", &[value()], Some(Type::CalledClass)),
            abstract_static_method(
                b"tryFrom",
                &[value()],
                Some(Type::Nullable(Box::new(Type::CalledClass))),
            ),
        ],
        ..declared(b"BackedEnum", ClassKind::Interface, None)
    }
}

/// `public static function name(...): returns;` on an interface:
/// abstract and not an instance member.
fn abstract_static_method(
    name: &[u8],
    parameters: &[Parameter],
    returns: Option<Type>,
) -> MethodDecl {
    MethodDecl {
        is_abstract: true,
        is_instance: false,
        ..throwable_method(name, parameters, returns)
    }
}

/// `class stdClass` as `zend_register_standard_class` declares it
/// (`Z/Zend/zend.c`, `zend_standard_class_def`): no parent, no
/// interface, no member, and `ZEND_ACC_ALLOW_DYNAMIC_PROPERTIES`, the
/// bit `#[\AllowDynamicProperties]` sets, so a name the class did not
/// declare is written without php's 8.2 deprecation; what `(object)`
/// builds and what `json_decode` and `unserialize` hand back.
///
/// ```php
/// var_dump(new stdClass, new stdClass instanceof stdClass);
/// ```
/// ```expect
/// object(stdClass)#1 (0) {
/// }
/// bool(true)
/// ```
///
/// The attribute is what the declaration is held to: a name the class
/// never declared is written on an instance and read back, and the
/// creation has nothing to say where a class without the bit raises
/// php's 8.2 deprecation.
///
/// ```php
/// $o = new stdClass;
/// $o->a = 1;
/// var_dump($o);
/// ```
/// ```expect
/// object(stdClass)#1 (1) {
///   ["a"]=>
///   int(1)
/// }
/// ```
fn std_class() -> ClassDecl {
    ClassDecl {
        flags: ClassFlags {
            dynamic_properties: DynamicProperties::Allowed,
            ..ClassFlags::default()
        },
        interfaces: Vec::new(),
        properties: Vec::new(),
        ..declared(b"stdClass", ClassKind::Class, None)
    }
}

/// `final class Closure` as `zend_register_closure_ce` declares it,
/// `ZEND_ACC_FINAL | ZEND_ACC_NO_DYNAMIC_PROPERTIES`, with the six
/// methods `class_Closure_methods` carries and no seventh:
/// [`ClosureHandlers::get_method`] answers `__invoke` as
/// `zend_closure_get_method` does, off the table.
///
/// `__construct` is the first row of the stub's table and stands here
/// with the declaration, with no entry to pair, as an interface's
/// methods do: php reaches its body through no lookup, since
/// `zend_closure_get_constructor` raises `Instantiation of class
/// Closure is not allowed` before `new` can find it, and the slot
/// `ZEND_NEW` reads is not the class's here yet.
fn closure_class() -> ClassDecl {
    ClassDecl {
        flags: ClassFlags {
            is_final: true,
            dynamic_properties: DynamicProperties::Forbidden,
            ..ClassFlags::default()
        },
        interfaces: Vec::new(),
        properties: Vec::new(),
        methods: vec![MethodDecl {
            visibility: Visibility::Private,
            ..closure_method(b"__construct", Vec::new(), None, true)
        }],
        ..declared(b"Closure", ClassKind::Class, None)
    }
}

/// `class_Closure_methods` (`Z/Zend/zend_closures_arginfo.h:37`) past
/// its first row, the five the stub declares after `__construct`, in
/// its order, each with the entry that serves it; the linker places
/// them after the declaration's own, so the linked table reads as the
/// stub writes it. `__invoke` is no row of this table in php and is
/// none here: it is [`ClosureHandlers::get_method`]'s answer, the
/// trampoline [`trampolines`] lays down.
fn closure_methods() -> Vec<(MethodDecl, Entry)> {
    let closure = || Type::Named(Name(b"Closure".as_slice().into()));
    let scope = || Parameter {
        default: Some(ConstExpr::Str(b"static".as_slice().into())),
        ..parameter(
            b"newScope",
            Type::Union(vec![Type::Object, Type::String, Type::Null]),
        )
    };
    vec![
        (
            closure_method(
                b"bind",
                vec![
                    parameter(b"closure", closure()),
                    parameter(b"newThis", Type::Nullable(Box::new(Type::Object))),
                    scope(),
                ],
                Some(Type::Nullable(Box::new(closure()))),
                false,
            ),
            closure_bind as Entry,
        ),
        (
            closure_method(
                b"bindTo",
                vec![
                    parameter(b"newThis", Type::Nullable(Box::new(Type::Object))),
                    scope(),
                ],
                Some(Type::Nullable(Box::new(closure()))),
                true,
            ),
            closure_bind_to as Entry,
        ),
        (
            closure_method(
                b"call",
                vec![parameter(b"newThis", Type::Object), variadic(b"args")],
                Some(Type::Mixed),
                true,
            ),
            closure_call as Entry,
        ),
        (
            closure_method(
                b"fromCallable",
                vec![parameter(b"callback", Type::Callable)],
                Some(closure()),
                false,
            ),
            closure_from_callable as Entry,
        ),
        (
            closure_method(b"getCurrent", Vec::new(), Some(closure()), false),
            closure_get_current as Entry,
        ),
    ]
}

/// `__invoke`, the one name a table here answers off the table.
const INVOKE: &[u8] = b"__invoke";

/// Every trampoline the engine lays down, `(class, method, entry)`:
/// php's `ZEND_ACC_CALL_VIA_HANDLER` function, which a class's
/// `get_method` synthesises on the lookup and no `ce->function_table`
/// carries, so no rendering and no lookup by name finds it.
/// `zend_get_closure_invoke_method` (`Z/Zend/zend_closures.c:476`)
/// makes the one there is: `Closure::__invoke`, public, an instance
/// member, and `ZEND_MN(Closure___invoke)` as its handler. php copies
/// the closure's own signature onto it, one function per lookup; here
/// one declaration stands for every closure, since the entry reads its
/// arguments off the frame and the payload's own declaration is what a
/// reflector and a diagnostic read.
pub(crate) fn trampolines() -> Vec<(Name, MethodDecl, Entry)> {
    vec![(
        Name(b"Closure".as_slice().into()),
        closure_method(INVOKE, vec![variadic(b"args")], Some(Type::Mixed), true),
        closure_invoke as Entry,
    )]
}

/// One method of `Closure` as the stub declares it: public, neither
/// abstract nor final, with `parameters` and `returns`, an instance
/// member or one on the class.
fn closure_method(
    name: &[u8],
    parameters: Vec<Parameter>,
    returns: Option<Type>,
    is_instance: bool,
) -> MethodDecl {
    MethodDecl {
        function: FunctionDecl {
            name: Name(name.into()),
            parameters,
            return_type: returns,
            returns_by_ref: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
            frameless: Vec::new(),
        },
        visibility: Visibility::Public,
        is_instance,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }
}

/// `type $name`: a required parameter, taken by value.
fn parameter(name: &[u8], ty: Type) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty: Some(ty),
        by_ref: false,
        prefer_ref: false,
        variadic: false,
        default: None,
        attributes: Vec::new(),
    }
}

/// `mixed ...$name`.
fn variadic(name: &[u8]) -> Parameter {
    Parameter {
        variadic: true,
        ..parameter(name, Type::Mixed)
    }
}

/// `Closure::fromCallable(callable $callback): Closure`,
/// `ZEND_METHOD(Closure, fromCallable)` (`Z/Zend/zend_closures.c:396`):
/// a closure handed in is handed back with one more owner; anything
/// else is resolved by [`Request::callable_checked`], php's
/// `zend_is_callable_ex` with its deprecations, and made a closure by
/// `zend_create_closure_from_callable`: `[$closure, '__invoke']` is the
/// closure itself, and every other callee a fake closure
/// (`zend_create_fake_closure`) over the function, the method's class
/// as its scope and the resolved receiver as its `$this`. What the
/// resolver refuses is php's `TypeError` with its reason.
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
/// Every shape the resolver reads makes one: a pair on an object keeps
/// the object as the closure's `$this`, a `static` method's closure has
/// none, a private method is reached from its own class's scope, and
/// `[$closure, '__invoke']` is the closure it names.
///
/// ```php
/// class C {
///     private $v = 'v';
///     function m($x) { return "m$x$this->v"; }
///     static function s($x) { return "s$x"; }
///     private function p() { return 'p'; }
///     function viaPrivate() { return Closure::fromCallable([$this, 'p']); }
/// }
/// $c = new C;
/// $m = Closure::fromCallable([$c, 'm']);
/// $s = Closure::fromCallable('C::s');
/// $inner = function () { return 'inner'; };
/// var_dump($m(1), $s(2), $c->viaPrivate()(), Closure::fromCallable([$inner, '__invoke']) === $inner, $m == Closure::fromCallable([$c, 'm']));
/// ```
/// ```expect
/// string(3) "m1v"
/// string(2) "s2"
/// string(1) "p"
/// bool(true)
/// bool(true)
/// ```
///
/// ```php
/// class C { private function p() {} }
/// try { Closure::fromCallable([new C, 'p']); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// Closure::fromCallable('nope');
/// ```
/// ```expect exit=255
/// Failed to create closure from callable: cannot access private method C::p()
///
/// Fatal error: Uncaught TypeError: Failed to create closure from callable: function "nope" not found or invalid function name in %s:4
/// Stack trace:
/// #0 %s(4): Closure::fromCallable('nope')
/// #1 {main}
///   thrown in %s on line 4
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
    let callable = match req.callable_checked(&given) {
        Ok(callable) => callable,
        Err(_) if req.is_pending() => return Err(Thrown::of(req.take_pending())),
        Err(reason) => {
            return Err(throw(
                req,
                "TypeError",
                &format!("Failed to create closure from callable: {reason}"),
            ));
        }
    };
    let (function, this, scope) = match callable.target() {
        Target::Closure(object) => {
            req.addref(&object.as_value());
            return Ok(object.as_value());
        }
        Target::Function(function) => (function, None, None),
        Target::Method { function, this, .. } => {
            if let Some(this) = this
                && this.closure(req).is_some()
                && req
                    .method_decl(function)
                    .is_some_and(|decl| decl.function.name.0.eq_ignore_ascii_case(INVOKE))
            {
                req.addref(&this.as_value());
                return Ok(this.as_value());
            }
            (function, this, req.method_class(function))
        }
    };
    let fake = ClosureInfo {
        function,
        this,
        scope,
        bound: Vec::new(),
        fake: true,
    };
    create_closure(req, &fake, scope, this).map(|made| made.as_value())
}

/// `Closure::__invoke(mixed ...$args): mixed`, `ZEND_METHOD(Closure,
/// __invoke)` (`Z/Zend/zend_closures.c:45`): the payload's function
/// served with the frame's arguments, the extra named ones handed on
/// as `Z_PARAM_VARIADIC_WITH_NAMED` hands them, on the payload's
/// receiver where the closure is bound and as a function where it is
/// not (`zend_call_known_fcc`); what it returns is the call's.
fn closure_invoke(req: &mut Request) -> Result<Value, Thrown> {
    let Some(this) = req.this() else {
        return Err(throw(
            req,
            "Error",
            "Closure::__invoke called with no receiver",
        ));
    };
    let arguments: Vec<Value> = (1..=u32::try_from(req.argument_count()).unwrap_or(u32::MAX))
        .filter_map(|position| req.argument(position))
        .collect();
    let named = req.named_arguments();
    req.serve_closure(this, &arguments, &named)
}

/// `private function __clone(): void {}`, as `Exception` and `Error`
/// declare it.
fn private_clone() -> MethodDecl {
    MethodDecl {
        visibility: Visibility::Private,
        ..throwable_method(b"__clone", &[], Some(Type::Void))
    }
}

/// One method of a throwable as the stub declares it: public, an
/// instance member, neither abstract nor final, with `parameters` and
/// `returns`.
fn throwable_method(name: &[u8], parameters: &[Parameter], returns: Option<Type>) -> MethodDecl {
    MethodDecl {
        function: FunctionDecl {
            name: Name(name.into()),
            parameters: parameters.to_vec(),
            return_type: returns,
            returns_by_ref: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
            frameless: Vec::new(),
        },
        visibility: Visibility::Public,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }
}

/// One abstract method of a Zend interface, with the parameters the stub
/// writes and the tentative return type `@tentative-return-type` marks:
/// a subclass that omits or narrows it is php's deprecation and not a
/// fatal, which is what `zend_interfaces.stub.php` gives every method of
/// `ArrayAccess`, `Iterator` and `Countable`.
fn interface_method(name: &[u8], parameters: &[Parameter], returns: Option<Type>) -> MethodDecl {
    MethodDecl {
        is_abstract: true,
        tentative_return: true,
        ..throwable_method(name, parameters, returns)
    }
}

/// The four methods `interface ArrayAccess` declares, in the stub's
/// order, each parameter `mixed` as it writes them.
fn array_access_methods() -> Vec<MethodDecl> {
    let offset = || Parameter {
        name: Name(b"offset".as_slice().into()),
        ty: Some(Type::Mixed),
        by_ref: false,
        prefer_ref: false,
        variadic: false,
        default: None,
        attributes: Vec::new(),
    };
    let value = Parameter {
        name: Name(b"value".as_slice().into()),
        ..offset()
    };
    vec![
        interface_method(b"offsetExists", &[offset()], Some(Type::Bool)),
        interface_method(b"offsetGet", &[offset()], Some(Type::Mixed)),
        interface_method(b"offsetSet", &[offset(), value], Some(Type::Void)),
        interface_method(b"offsetUnset", &[offset()], Some(Type::Void)),
    ]
}

/// A `final public` method of a throwable, the shape the seven getters
/// of `Exception` and `Error` share.
fn final_method(name: &[u8], returns: Option<Type>) -> MethodDecl {
    MethodDecl {
        is_final: true,
        ..throwable_method(name, &[], returns)
    }
}

/// `public function name(): returns;` on an interface: abstract, as
/// every interface method is (`ZEND_ACC_ABSTRACT` on each entry of an
/// interface's table).
fn abstract_method(name: &[u8], returns: Option<Type>) -> MethodDecl {
    MethodDecl {
        is_abstract: true,
        ..throwable_method(name, &[], returns)
    }
}

/// The seven methods `interface Throwable` declares, in the stub's
/// order; `getCode` declares no return type, the stub's `@return int`
/// being a comment (its `TODO add proper type`).
fn throwable_abstract_methods() -> Vec<MethodDecl> {
    let throwable = || Type::Nullable(Box::new(Type::Named(Name::from("Throwable"))));
    vec![
        abstract_method(b"getMessage", Some(Type::String)),
        abstract_method(b"getCode", None),
        abstract_method(b"getFile", Some(Type::String)),
        abstract_method(b"getLine", Some(Type::Int)),
        abstract_method(b"getTrace", Some(Type::Array)),
        abstract_method(b"getPrevious", Some(throwable())),
        abstract_method(b"getTraceAsString", Some(Type::String)),
    ]
}

/// The eleven methods `Exception` and `Error` declare, each beside its
/// entry, in the stub's order: the private `__clone`, the constructor,
/// `__wakeup` with its tentative `void`, the seven final getters and
/// `__toString`.
fn throwable_methods() -> Vec<(MethodDecl, Entry)> {
    let string_parameter = |name: &[u8], ty: Type, default: ConstExpr| Parameter {
        name: Name(name.into()),
        ty: Some(ty),
        by_ref: false,
        prefer_ref: false,
        variadic: false,
        default: Some(default),
        attributes: Vec::new(),
    };
    let throwable = || Type::Nullable(Box::new(Type::Named(Name::from("Throwable"))));
    vec![
        (private_clone(), throwable_clone as Entry),
        (
            throwable_method(
                b"__construct",
                &[
                    string_parameter(b"message", Type::String, ConstExpr::Str(Box::new([]))),
                    string_parameter(b"code", Type::Int, ConstExpr::Long(0)),
                    string_parameter(b"previous", throwable(), ConstExpr::Null),
                ],
                None,
            ),
            throwable_construct as Entry,
        ),
        (
            MethodDecl {
                tentative_return: true,
                ..throwable_method(b"__wakeup", &[], Some(Type::Void))
            },
            throwable_wakeup as Entry,
        ),
        (
            final_method(b"getMessage", Some(Type::String)),
            throwable_get_message as Entry,
        ),
        (final_method(b"getCode", None), throwable_get_code as Entry),
        (
            final_method(b"getFile", Some(Type::String)),
            throwable_get_file as Entry,
        ),
        (
            final_method(b"getLine", Some(Type::Int)),
            throwable_get_line as Entry,
        ),
        (
            final_method(b"getTrace", Some(Type::Array)),
            throwable_get_trace as Entry,
        ),
        (
            final_method(b"getPrevious", Some(throwable())),
            throwable_get_previous as Entry,
        ),
        (
            final_method(b"getTraceAsString", Some(Type::String)),
            throwable_get_trace_as_string as Entry,
        ),
        (
            throwable_method(b"__toString", &[], Some(Type::String)),
            throwable_to_string as Entry,
        ),
    ]
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

/// `ZEND_THIS` for a throwable's method: the receiver, or php's `Error`
/// for a call that reached the entry with none.
fn this_throwable(req: &mut Request) -> Result<PhpObject, Thrown> {
    req.this().ok_or_else(|| {
        throw(
            req,
            "Error",
            "Internal error: Failed to retrieve the throwable receiver",
        )
    })
}

/// `ZEND_METHOD(Exception, __construct)`,
/// `Exception::__construct(string $message = "", int $code = 0,
/// ?Throwable $previous = null)`: `zend_parse_parameters` over
/// `|SlO!`, then `zend_update_exception_properties` writes the three
/// it was given onto the receiver, `message`, `code` and `previous`,
/// and leaves the property's default where an argument is absent.
/// `Error::__construct` is the stub's `@implementation-alias` of it.
///
/// ```php
/// $e = new Exception("boom", 3, new Error("first")); var_dump($e->getMessage(), $e->getCode(), $e->getPrevious()->getMessage(), (new Exception)->getMessage(), (new Exception(12))->getMessage(), (new Error("e", "7"))->getCode());
/// ```
/// ```expect
/// string(4) "boom"
/// int(3)
/// string(5) "first"
/// string(0) ""
/// string(2) "12"
/// int(7)
/// ```
///
/// ```php
/// new Exception("x", 0, new stdClass);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Exception::__construct(): Argument #3 ($previous) must be of type ?Throwable, stdClass given in %s:2
/// Stack trace:
/// #0 %s(2): Exception->__construct('x', 0, Object(stdClass))
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new Error("x", 0, null, 4);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: Error::__construct() expects at most 3 arguments, 4 given in %s:2
/// Stack trace:
/// #0 %s(2): Error->__construct('x', 0, NULL, 4)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn throwable_construct(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count > 3 {
        return Err(arg_count_error(
            req,
            &format!("at most 3 arguments, {count} given"),
        ));
    }
    let this = this_throwable(req)?;
    if let Some(message) = req.argument(1) {
        let message = coerce_param(req, &message, &Type::String, 1)?;
        req.addref(&message);
        write_throwable_property(req, this, b"message", message)?;
    }
    if let Some(code) = req.argument(2) {
        let code = coerce_param(req, &code, &Type::Int, 2)?;
        write_throwable_property(req, this, b"code", code)?;
    }
    if let Some(previous) = req.argument(3) {
        let previous = previous.deref();
        let is_throwable = match previous.as_object() {
            Some(object) => req
                .class_named(b"Throwable")
                .is_some_and(|throwable| req.instance_of(object.class(), throwable)),
            None => matches!(previous.as_ref(), ValueRef::Null | ValueRef::Undef),
        };
        if !is_throwable {
            // `zend_zval_value_name`: an object is named by its class.
            let given = previous.as_object().map_or_else(
                || String::from(previous.type_name()),
                |object| req.class_name(object.class()),
            );
            return Err(arg_type_error(
                req,
                3,
                &format!("be of type ?Throwable, {given} given"),
            ));
        }
        req.addref(&previous);
        write_throwable_property(req, this, b"previous", previous)?;
    }
    Ok(Value::null())
}

/// `zend_update_property_ex` on the throwable's base class: the value
/// written by name, one owner of it the object's.
fn write_throwable_property(
    req: &mut Request,
    this: PhpObject,
    name: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    let name = PhpString::new(req, name)?;
    this.set_prop(req, name, value)
}

/// `ZEND_METHOD(Exception, __wakeup)`: after `unserialize`, a `message`
/// that is not a string and a `code` that is not an int are unset, the
/// typed properties being held by their types; a throwable this engine
/// built holds both as declared, so there is nothing to unset yet, and
/// `unserialize` is what makes the check reachable.
///
/// ```php
/// $e = new Exception("m"); var_dump($e->__wakeup(), $e->getMessage());
/// ```
/// ```expect
/// NULL
/// string(1) "m"
/// ```
fn throwable_wakeup(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    Ok(Value::null())
}

/// `ZEND_PARSE_PARAMETERS_NONE()` for a getter: php's
/// `ArgumentCountError`, `expects exactly 0 arguments, N given`.
fn no_arguments(req: &mut Request) -> Result<(), Thrown> {
    let count = req.argument_count();
    if count == 0 {
        return Ok(());
    }
    Err(arg_count_error(
        req,
        &format!("exactly 0 arguments, {count} given"),
    ))
}

/// `DEFAULT_0_PARAMS` and `GET_PROPERTY(ZEND_THIS, name)`: the getter's
/// property read off the receiver, `null` where the slot was never
/// written, with one more owner for the return.
fn throwable_property(req: &mut Request, name: &[u8]) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let this = this_throwable(req)?;
    let value = this.prop(req, name).unwrap_or_default();
    req.addref(&value);
    Ok(value)
}

/// `ZEND_METHOD(Exception, getMessage)`: the `message` property. The
/// witness is the constructor's.
fn throwable_get_message(req: &mut Request) -> Result<Value, Thrown> {
    throwable_property(req, b"message")
}

/// `ZEND_METHOD(Exception, getCode)`: the `code` property. The witness
/// is the constructor's.
fn throwable_get_code(req: &mut Request) -> Result<Value, Thrown> {
    throwable_property(req, b"code")
}

/// `ZEND_METHOD(Exception, getFile)` and `getLine`: the site
/// `zend_default_exception_new` recorded, which is where `new` ran.
///
/// ```php
/// $e = new Exception; var_dump($e->getLine(), $e->getFile());
/// ```
/// ```expect
/// int(2)
/// string(%d) "%s.php"
/// ```
fn throwable_get_file(req: &mut Request) -> Result<Value, Thrown> {
    throwable_property(req, b"file")
}

/// `ZEND_METHOD(Exception, getLine)`: the `line` property. The witness
/// is `getFile`'s.
fn throwable_get_line(req: &mut Request) -> Result<Value, Thrown> {
    throwable_property(req, b"line")
}

/// `ZEND_METHOD(Exception, getTrace)`: the `trace` property, the frames
/// `zend_fetch_debug_backtrace` recorded at the site
/// (`Request::note_throw_site`), one more owner taken for the return;
/// the empty array where the slot holds none. A throwable made at the
/// top level has no frame below it.
///
/// ```php
/// var_dump((new Exception)->getTrace());
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
///
/// One made inside a user function has a row per frame below it, the
/// innermost first, each at the site the frame was called from: a
/// method's row names its `class` and `type`, and `args` are the
/// frame's, a by-reference parameter followed through the reference
/// and a variadic's extras after the declared ones.
///
/// ```php
/// class C { function m($a, &$r) { $r = 1; return new Exception('m'); } }
/// function f(int ...$rest) { $x = 0; return (new C)->m('q', $x); }
/// var_dump(f(1, 2)->getTrace());
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   array(6) {
///     ["file"]=>
///     string(%d) "%s.php"
///     ["line"]=>
///     int(3)
///     ["function"]=>
///     string(1) "m"
///     ["class"]=>
///     string(1) "C"
///     ["type"]=>
///     string(2) "->"
///     ["args"]=>
///     array(2) {
///       [0]=>
///       string(1) "q"
///       [1]=>
///       int(1)
///     }
///   }
///   [1]=>
///   array(4) {
///     ["file"]=>
///     string(%d) "%s.php"
///     ["line"]=>
///     int(4)
///     ["function"]=>
///     string(1) "f"
///     ["args"]=>
///     array(2) {
///       [0]=>
///       int(1)
///       [1]=>
///       int(2)
///     }
///   }
/// }
/// ```
///
/// One made inside a call a native serves has that call's row with no
/// `file` and no `line`, since the link below it is the native and not
/// user code, and then the native's own row at the opline that called
/// it. `var_dump` over a `__debugInfo` that answers the trace prints it
/// as the object's rows, and the object among `var_dump`'s `args` is
/// the one being dumped, `*RECURSION*`.
///
/// ```php
/// class C { function __debugInfo() { return (new Exception)->getTrace(); } }
/// var_dump(new C);
/// ```
/// ```expect
/// object(C)#1 (2) {
///   [0]=>
///   array(4) {
///     ["function"]=>
///     string(11) "__debugInfo"
///     ["class"]=>
///     string(1) "C"
///     ["type"]=>
///     string(2) "->"
///     ["args"]=>
///     array(0) {
///     }
///   }
///   [1]=>
///   array(4) {
///     ["file"]=>
///     string(%d) "%s.php"
///     ["line"]=>
///     int(3)
///     ["function"]=>
///     string(8) "var_dump"
///     ["args"]=>
///     array(1) {
///       [0]=>
///       *RECURSION*
///     }
///   }
/// }
/// ```
fn throwable_get_trace(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let this = this_throwable(req)?;
    if let Some(trace) = this
        .prop(req, b"trace")
        .filter(|value| value.as_array().is_some())
    {
        req.addref(&trace);
        return Ok(trace);
    }
    Ok(Value::array(PhpArray::new(req)?))
}

/// `ZEND_METHOD(Exception, getPrevious)`: the `previous` property. The
/// witness is the constructor's.
fn throwable_get_previous(req: &mut Request) -> Result<Value, Thrown> {
    throwable_property(req, b"previous")
}

/// `ZEND_METHOD(Exception, getTraceAsString)`: `zend_trace_to_string`
/// over the recorded frames, `Request::trace_string_of`; `#0 {main}` is
/// the rendering of no frame.
///
/// ```php
/// var_dump((new Exception)->getTraceAsString());
/// ```
/// ```expect
/// string(9) "#0 {main}"
/// ```
///
/// The frames a throwable made inside user functions holds render one
/// line each, the innermost first, and the uncaught rendering of the
/// same throwable reads the same array.
///
/// ```php
/// function g() { throw new Exception('deep'); }
/// function f() { g(); }
/// try { f(); } catch (Exception $e) { var_dump($e->getTraceAsString()); }
/// f();
/// ```
/// ```expect exit=255
/// string(%d) "#0 %s(3): g()
/// #1 %s(4): f()
/// #2 {main}"
///
/// Fatal error: Uncaught Exception: deep in %s:2
/// Stack trace:
/// #0 %s(3): g()
/// #1 %s(5): f()
/// #2 {main}
///   thrown in %s on line 2
/// ```
fn throwable_get_trace_as_string(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let this = this_throwable(req)?;
    let trace = req.trace_string_of(this);
    Ok(Value::string(PhpString::new(req, trace.as_bytes())?))
}

/// `ZEND_METHOD(Exception, __toString)`: the class, the message, the
/// site and the trace, as an uncaught one prints under `Uncaught`.
///
/// ```php
/// echo new Exception("boom");
/// ```
/// ```expect
/// Exception: boom in %s:2
/// Stack trace:
/// #0 {main}
/// ```
fn throwable_to_string(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let this = this_throwable(req)?;
    let text = req.throwable_to_string(this);
    Ok(Value::string(PhpString::new(req, text.as_bytes())?))
}

fn declared(name: &[u8], kind: ClassKind, parent: Option<&[u8]>) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind,
        flags: ClassFlags::default(),
        parent: parent.map(|p| Name(p.into())),
        // php declares `Throwable` on `Exception` and `Error` alone and
        // the rest inherit it (`zend_register_default_classes`); the
        // linker is what carries it down (design §6.2). `Stringable` is
        // never written: `zend_register_functions` adds it to a class
        // that declares `__toString`, and `Throwable` extends it.
        interfaces: match (kind, parent) {
            (ClassKind::Class, None) => vec![Name(b"Throwable".as_slice().into())],
            (ClassKind::Interface, None) if name == b"Throwable" => {
                vec![Name(b"Stringable".as_slice().into())]
            }
            _ => Vec::new(),
        },
        traits: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        // php declares the seven properties on `Exception` and `Error`
        // alike (`zend_exceptions.stub.php`), and a child inherits them;
        // `stdClass` and `Closure` set theirs back to none.
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
        set_visibility: None,
        is_instance: true,
        is_readonly: false,
        is_final: false,
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

/// The `object|string|null $newScope` of `bind` and `bindTo`,
/// `Z_PARAM_OBJ_OR_STR_OR_NULL` with php's default `"static"`.
enum NewScope {
    Object(PhpObject),
    Named(Vec<u8>),
    Null,
}

/// The receiver and scope arguments of `bind` from `first` on, and of
/// `bindTo` from one: `?object $newThis` then the scope, read as the
/// stub's types read them.
fn binding_arguments(
    req: &mut Request,
    first: u32,
) -> Result<(Option<PhpObject>, NewScope), Thrown> {
    let this = req.argument(first).unwrap_or_default();
    let this = coerce_param(req, &this, &Type::Nullable(Box::new(Type::Object)), first)?;
    let this = this.deref().as_object().copied();
    let count = u32::try_from(req.argument_count()).unwrap_or(u32::MAX);
    if count <= first {
        return Ok((this, NewScope::Named(b"static".to_vec())));
    }
    let scope = req.argument(first + 1).unwrap_or_default();
    let scope = coerce_param(
        req,
        &scope,
        &Type::Union(vec![Type::Object, Type::String, Type::Null]),
        first + 1,
    )?;
    let scope = scope.deref();
    let scope = if let Some(object) = scope.as_object().copied() {
        NewScope::Object(object)
    } else if let Some(name) = scope.as_str_bytes() {
        NewScope::Named(name.to_vec())
    } else {
        NewScope::Null
    };
    Ok((this, scope))
}

/// `Closure::bind(Closure $closure, ?object $newThis, object|string|null $newScope = "static"): ?Closure`,
/// `ZEND_METHOD(Closure, bind)`
/// (`Z/Zend/zend_closures.c:354`): [`do_closure_bind`] over the closure
/// handed in.
///
/// ```php
/// class A { private $x = 1; }
/// class B extends A {}
/// $get = function () { return $this->x; };
/// $bound = Closure::bind($get, new A, 'A');
/// var_dump($bound());
/// $fromB = Closure::bind($get, new B, 'A');
/// var_dump($fromB());
/// try { Closure::bind($get, new A)(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump(Closure::bind($get, null, 'Nope'));
/// ```
/// ```expect
/// int(1)
/// int(1)
/// Cannot access private property A::$x
///
/// Warning: Class "Nope" not found in %s on line 10
/// NULL
/// ```
fn closure_bind(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count < 2 {
        return Err(arg_count_error(
            req,
            &format!("at least 2 arguments, {count} given"),
        ));
    }
    if count > 3 {
        return Err(arg_count_error(
            req,
            &format!("at most 3 arguments, {count} given"),
        ));
    }
    let closure = req.argument(1).unwrap_or_default();
    let closure = coerce_param(
        req,
        &closure,
        &Type::Named(Name(b"Closure".as_slice().into())),
        1,
    )?;
    let Some(closure) = closure.deref().as_object().copied() else {
        return Ok(Value::null());
    };
    let (this, scope) = binding_arguments(req, 2)?;
    do_closure_bind(req, closure, this, scope)
}

/// `Closure::bindTo(?object $newThis, object|string|null $newScope = "static"): ?Closure`,
/// `ZEND_METHOD(Closure, bindTo)`
/// (`Z/Zend/zend_closures.c:371`): [`do_closure_bind`] over the
/// receiver. A copy is made each time, of the function, the variables
/// it bound and a new receiver and scope, and the original is left as
/// it was.
///
/// ```php
/// class C { public $v = 'c'; function get() { return function () { return $this->v; }; } }
/// class D { public $v = 'd'; }
/// $n = 5;
/// $f = (new C)->get();
/// $g = $f->bindTo(new D);
/// $h = function () use ($n) { return $n; };
/// var_dump($f(), $g(), $h->bindTo(null)(), $f === $g);
/// ```
/// ```expect
/// string(1) "c"
/// string(1) "d"
/// int(5)
/// bool(false)
/// ```
///
/// Each of `zend_valid_closure_binding`'s refusals is a warning and
/// `null`, the closure left unbound.
///
/// ```php
/// class C { function m() {} static function s() {} }
/// $static = static function () {};
/// var_dump($static->bindTo(new C));
/// $uses = (function () { return $this; })->bindTo(new C);
/// var_dump($uses->bindTo(null));
/// var_dump((function () {})->bindTo(null, 'Exception'));
/// $method = Closure::fromCallable([new C, 'm']);
/// var_dump($method->bindTo(new stdClass));
/// var_dump($method->bindTo(null));
/// var_dump(Closure::fromCallable('strlen')->bindTo(null, 'C'));
/// var_dump(Closure::fromCallable('C::s')->bindTo(null, 'stdClass'));
/// ```
/// ```expect
/// Warning: Cannot bind an instance to a static closure, this will be an error in PHP 9 in %s on line 4
/// NULL
///
/// Warning: Cannot unbind $this of closure using $this, this will be an error in PHP 9 in %s on line 6
/// NULL
///
/// Warning: Cannot bind closure to scope of internal class Exception, this will be an error in PHP 9 in %s on line 7
/// NULL
///
/// Warning: Cannot bind method C::m() to object of class stdClass, this will be an error in PHP 9 in %s on line 9
/// NULL
///
/// Warning: Cannot unbind $this of method, this will be an error in PHP 9 in %s on line 10
/// NULL
///
/// Warning: Cannot rebind scope of closure created from function, this will be an error in PHP 9 in %s on line 11
/// NULL
///
/// Warning: Cannot bind closure to scope of internal class stdClass, this will be an error in PHP 9 in %s on line 12
/// NULL
/// ```
fn closure_bind_to(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count < 1 {
        return Err(arg_count_error(
            req,
            &format!("at least 1 argument, {count} given"),
        ));
    }
    if count > 2 {
        return Err(arg_count_error(
            req,
            &format!("at most 2 arguments, {count} given"),
        ));
    }
    let Some(closure) = req.this() else {
        return Ok(Value::null());
    };
    let (this, scope) = binding_arguments(req, 1)?;
    do_closure_bind(req, closure, this, scope)
}

/// `do_closure_bind` (`Z/Zend/zend_closures.c:323`): the scope the
/// arguments name, an object's class, `"static"` the closure's own and
/// any other string the class of that name, php's warning and `null`
/// for one nothing declares; the binding held to
/// [`valid_closure_binding`]; then `zend_create_closure` over the
/// closure's function with the new receiver and scope, a receiver with
/// no scope taking `Closure` as its dummy scope as php's does.
fn do_closure_bind(
    req: &mut Request,
    closure: PhpObject,
    this: Option<PhpObject>,
    scope: NewScope,
) -> Result<Value, Thrown> {
    let Some(info) = closure.closure(req).cloned() else {
        return Ok(Value::null());
    };
    let scope = match scope {
        NewScope::Object(object) => Some(object.class()),
        NewScope::Named(name) if name == b"static" => info.scope,
        NewScope::Named(name) => {
            let Some(class) = req.class_named(name.strip_prefix(b"\\").unwrap_or(&name)) else {
                warning(
                    req,
                    &format!("Class \"{}\" not found", String::from_utf8_lossy(&name)),
                );
                return Ok(Value::null());
            };
            Some(class)
        }
        NewScope::Null => None,
    };
    if !valid_closure_binding(req, &info, this, scope) {
        return Ok(Value::null());
    }
    let scope = scope.or_else(|| this.and_then(|_| req.class_named(b"Closure")));
    create_closure(req, &info, scope, this).map(|made| made.as_value())
}

/// `zend_create_closure` (`Z/Zend/zend_closures.c:855`) over the
/// function a closure holds: a new `Closure` with the given receiver
/// and scope, the receiver one more owner, and a copy of the variables
/// it bound, each one more owner, as `zend_array_dup` makes it.
fn create_closure(
    req: &mut Request,
    info: &ClosureInfo,
    scope: Option<ClassId>,
    this: Option<PhpObject>,
) -> Result<PhpObject, Thrown> {
    let Some(class) = req.class_named(b"Closure") else {
        return Err(throw(req, "Error", "Class \"Closure\" not found"));
    };
    if let Some(this) = this {
        req.addref(&this.as_value());
    }
    for bound in &info.bound {
        req.addref(bound);
    }
    req.new_object_with(
        class,
        ClosureInfo {
            function: info.function,
            this,
            scope,
            bound: info.bound.clone(),
            fake: info.fake,
        },
    )
}

/// `zend_valid_closure_binding` (`Z/Zend/zend_closures.c:79`): whether
/// the closure may take `this` and `scope`, each refusal php's warning
/// in its words. A `static` closure takes no receiver; a closure made of
/// a method takes only one of the method's class, keeps one where the
/// method is an instance method, and keeps its scope; a closure that
/// uses `$this` keeps its receiver; and no closure is bound into an
/// internal class's scope.
fn valid_closure_binding(
    req: &mut Request,
    info: &ClosureInfo,
    this: Option<PhpObject>,
    scope: Option<ClassId>,
) -> bool {
    let is_static = req.closure_is_static(info);
    if let Some(this) = this {
        if is_static {
            warning(
                req,
                "Cannot bind an instance to a static closure, this will be an error in PHP 9",
            );
            return false;
        }
        if info.fake
            && let Some(own) = info.scope
            && !req.instance_of(this.class(), own)
        {
            let method = req.function_name(info.function);
            let message = format!(
                "Cannot bind method {}::{method}() to object of class {}, this will be an error in PHP 9",
                req.class_name(own),
                req.class_name(this.class())
            );
            warning(req, &message);
            return false;
        }
    } else if info.fake && info.scope.is_some() && !is_static {
        warning(
            req,
            "Cannot unbind $this of method, this will be an error in PHP 9",
        );
        return false;
    } else if !info.fake && info.this.is_some() && req.closure_uses_this(info) {
        warning(
            req,
            "Cannot unbind $this of closure using $this, this will be an error in PHP 9",
        );
        return false;
    }
    if let Some(scope) = scope
        && Some(scope) != info.scope
        && req.class_is_internal(scope)
    {
        let message = format!(
            "Cannot bind closure to scope of internal class {}, this will be an error in PHP 9",
            req.class_name(scope)
        );
        warning(req, &message);
        return false;
    }
    if info.fake && scope != info.scope {
        let made = if info.scope.is_none() {
            "function"
        } else {
            "method"
        };
        warning(
            req,
            &format!(
                "Cannot rebind scope of closure created from {made}, this will be an error in PHP 9"
            ),
        );
        return false;
    }
    true
}

/// `Closure::call(object $newThis, mixed ...$args): mixed`,
/// `ZEND_METHOD(Closure, call)` (`Z/Zend/zend_closures.c:138`): the
/// closure's function served once on `$newThis`, in the scope of its
/// class, with the rest of the arguments, the closure itself left bound
/// as it was; a binding [`valid_closure_binding`] refuses is its
/// warning and `null`. What the call returns by reference is unwrapped.
///
/// ```php
/// class Foo { private $x = 3; }
/// $get = function ($m) { return $this->x * $m; };
/// var_dump($get->call(new Foo, 7));
/// $o = new stdClass; $o->x = 7;
/// var_dump((function () { return $this->x; })->call($o));
/// ```
/// ```expect
/// int(21)
///
/// Warning: Cannot bind closure to scope of internal class stdClass, this will be an error in PHP 9 in %s on line 6
/// NULL
/// ```
fn closure_call(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count < 1 {
        return Err(arg_count_error(
            req,
            &format!("at least 1 argument, {count} given"),
        ));
    }
    let Some(closure) = req.this() else {
        return Ok(Value::null());
    };
    let this = req.argument(1).unwrap_or_default();
    let this = coerce_param(req, &this, &Type::Object, 1)?;
    let Some(this) = this.deref().as_object().copied() else {
        return Ok(Value::null());
    };
    let Some(info) = closure.closure(req).cloned() else {
        return Ok(Value::null());
    };
    let class = this.class();
    if !valid_closure_binding(req, &info, Some(this), Some(class)) {
        return Ok(Value::null());
    }
    let arguments: Vec<Value> = (2..=u32::try_from(count).unwrap_or(u32::MAX))
        .filter_map(|position| req.argument(position))
        .collect();
    let named = req.named_arguments();
    let returned = req.serve_closure_as(closure, this, class, &arguments, &named)?;
    Ok(returned.deref())
}

/// `Closure::getCurrent(): Closure` (8.5), `ZEND_METHOD(Closure,
/// getCurrent)` (`Z/Zend/zend_closures.c:421`): the closure whose body
/// called it, one more owner, and php's `Error` where the caller is no
/// closure, a closure made of a named function included.
///
/// ```php
/// $fact = function ($n) { return $n <= 1 ? 1 : $n * Closure::getCurrent()($n - 1); };
/// var_dump($fact(5));
/// try { Closure::getCurrent(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(120)
/// Current function is not a closure
/// ```
fn closure_get_current(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let Some(closure) = req.calling_closure() else {
        return Err(throw(req, "Error", "Current function is not a closure"));
    };
    req.addref(&closure.as_value());
    Ok(closure.as_value())
}
