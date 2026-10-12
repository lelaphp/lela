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

use php_unit::Level;
use php_unit::decl::{Bare, FunctionDecl, MethodDecl, Name, Type};

use php_macros::php_class;

use crate::api::__private::native_function;
use crate::api::abi::{
    Callable, CloneObj, ConstArray, DebugInfo, Entry, FromArgument, GcTable, Instance,
    NamedVariadic, ObjectHandlers, Passed, Raw, Static, This, Union, Untyped,
};
use crate::api::call::{Function, Target, write_prop};
use crate::api::raising::{Thrown, throw, throw_error, throw_over_pending, warning};
use crate::api::request::{Refusal, Request};
use crate::api::values::{
    ClassId, ClosureInfo, Key, Owner, PhpArray, PhpObject, PhpString, Value, adopted, adopted_some,
};
use crate::trampoline::{public_method, variadic};

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

/// Whether `handlers` is a Core table whose class php gives a
/// `create_object` with no native payload here: the throwables'
/// `zend_default_exception_new` and `Closure`'s `zend_closure_new`.
pub(crate) fn core_create_object(handlers: &'static dyn ObjectHandlers) -> bool {
    records_throw_site(handlers) || std::ptr::addr_eq(handlers, &raw const CLOSURE)
}

/// `closure_handlers` (`Z/Zend/zend_closures.c:730`): the slots
/// `zend_register_closure_ce` overrides over `std_object_handlers`.
struct ClosureHandlers;

impl ObjectHandlers for ClosureHandlers {
    /// `zend_closure_get_constructor`: a closure is made by the engine
    /// alone.
    ///
    /// ```php
    /// try { new Closure; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Instantiation of class Closure is not allowed
    /// ```
    ///
    /// The refusal is raised before any frame is pushed, so the trace
    /// names no constructor:
    ///
    /// ```php
    /// new Closure();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Instantiation of class Closure is not allowed in %s:2
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 2
    /// ```
    fn get_constructor(&self, req: &mut Request, _: PhpObject) -> Result<Option<Function>, Thrown> {
        Err(throw_error(
            req,
            "Error",
            "Instantiation of class Closure is not allowed",
        ))
    }

    /// `zend_closure_free_storage`: `zend_object_std_dtor` first, which
    /// tells the weak references, then the bound variables and the bound
    /// `$this` given back; the payload's drop is the heap's.
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { echo $this->n, "\n"; } }
    /// class T { function f() { return function () {}; } function __destruct() { echo "this\n"; } }
    /// $c = (new T)->f(); $wm = new WeakMap; $wm[$c] = new D('map value'); unset($c);
    /// ```
    /// ```expect
    /// map value
    /// this
    /// ```
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        req.free_members(object);
        if let Some(bound) = object
            .native_mut::<ClosureInfo>(req)
            .and_then(|closure| closure.bound.take())
        {
            req.release(Value::array(bound));
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
    /// `ZEND_MAP_PTR_GET(closure->func.op_array.static_variables_ptr)`
    /// and returns as the object's table, so a cycle through a `use`
    /// variable collects the object and the closure and no more:
    ///
    /// ```php
    /// class Bar { public $x; function __construct() { $self = $this; $this->x = function () use ($self) {}; } }
    /// gc_collect_cycles();
    /// new Bar;
    /// var_dump(gc_collect_cycles());
    /// ```
    /// ```expect
    /// int(2)
    /// ```
    ///
    /// The table is walked after `$this`, as php walks a returned table
    /// after the values its handler names, so what the bound variables
    /// reach is found before what `$this` reaches, and its destructors
    /// are served first:
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { echo $this->n, "\n"; } }
    /// class H { public $d; public $f; }
    /// function make() { $t = new H; $t->d = new D('this'); $a = new D('a'); $b = new D('b'); return $t->f = Closure::bind(function () use ($a, $b) {}, $t, H::class); }
    /// $g = make();
    /// gc_collect_cycles();
    /// unset($g);
    /// gc_collect_cycles();
    /// echo "done\n";
    /// ```
    /// ```expect
    /// a
    /// b
    /// this
    /// done
    /// ```
    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        let Some(closure) = object.closure(req) else {
            return GcTable::Properties;
        };
        if let Some(this) = closure.this {
            visit(this.as_value());
        }
        closure.bound.map_or(GcTable::Properties, GcTable::Owned)
    }

    /// `zend_closure_get_closure`: the payload itself.
    fn get_closure(&self, req: &mut Request, object: PhpObject) -> Option<ClosureInfo> {
        object.closure(req).cloned()
    }

    /// `zend_closure_get_method` (`Z/Zend/zend_closures.c:528`):
    /// `__invoke` is the function `zend_get_closure_invoke_method`
    /// makes on the lookup ([`Request::invoke_trampoline`]), which
    /// `Closure`'s table has no row for and which carries the closure's
    /// own signature, and every other name falls to
    /// `zend_std_get_method`. So `$f->__invoke()` reaches the same entry
    /// `$f()` reaches, sending by reference where the closure takes a
    /// reference, and nothing that reads the table, a rendering or a
    /// reflector, sees a seventh method on the class.
    ///
    /// ```php
    /// $inc = function (&$n) { $n++; };
    /// $n = 1;
    /// $inc->__INVOKE($n);
    /// var_dump($n);
    /// call_user_func([$inc, '__invoke'], $n);
    /// ```
    /// ```expect
    /// int(2)
    ///
    /// Warning: Closure::__invoke(): Argument #1 ($n) must be passed by reference, value given in %s on line 6
    /// ```
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
    fn get_method(
        &self,
        req: &mut Request,
        object: PhpObject,
        lowered: &[u8],
    ) -> Result<Option<Function>, Thrown> {
        if lowered.eq_ignore_ascii_case(INVOKE) {
            return Ok(req.invoke_trampoline(object));
        }
        Ok(req.method_named(object.class(), lowered))
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
    /// A body's `static`s follow the `use` variables in the same table,
    /// each shown as its value once the call that bound it is over:
    ///
    /// ```php
    /// $b = 5;
    /// $f = function () use ($b) { static $n = 1; $n++; };
    /// $f();
    /// print_r($f);
    /// ```
    /// ```expect
    /// Closure Object
    /// (
    ///     [name] => {closure:%s:3}
    ///     [file] => %s
    ///     [line] => 3
    ///     [static] => Array
    ///         (
    ///             [b] => 5
    ///             [n] => 2
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
    ) -> Result<Option<DebugInfo>, Thrown> {
        let Some(closure) = object.closure(req).cloned() else {
            return Ok(None);
        };
        let Some(decl) = req.function_decl(closure.function).cloned() else {
            return PhpArray::new(req).map(|table| Some(table.into()));
        };
        let out = PhpArray::filled(req, |req, out| {
            closure_rows(req, out, object, &closure, &decl)
        })?;
        Ok(Some(out.into()))
    }

    /// `zend_closure_compare` (`Z/Zend/zend_closures.c:446`): two fake
    /// closures are equal when they carry one receiver, one called
    /// scope, and functions of one kind, one scope and one name; any
    /// other pair is uncomparable, a closure the program declared
    /// included, so two of them are equal only where `zend_compare`
    /// found one object on both sides before the handler was reached. A
    /// side that is no closure is `ZEND_COMPARE_OBJECTS_FALLBACK`,
    /// `zend_std_compare_objects`.
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
    ///
    /// A method of the class called on two classes differs by its called
    /// scope, and two closures over one name `__call` answers are equal:
    ///
    /// ```php
    /// class P { static function s() {} function __call($m, $a) {} }
    /// class C extends P {}
    /// $p = new P;
    /// var_dump(P::s(...) == C::s(...), P::s(...) == P::s(...), $p->nope(...) == $p->nope(...), $p->nope(...) == $p->other(...));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
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
        let kind = |function| req.user_function(function).is_some();
        let same = x.this == y.this
            && x.called_scope == y.called_scope
            && kind(x.function) == kind(y.function)
            && req.method_class(x.function) == req.method_class(y.function)
            && req.function_name(x.function) == req.function_name(y.function);
        Ok(same.then_some(Ordering::Equal))
    }
}

/// The rows of a closure's debug table, `zend_closure_get_debug_info`,
/// into `out`; each nested table is given up where a step of it throws.
fn closure_rows(
    req: &mut Request,
    out: &mut Owner<PhpArray>,
    object: PhpObject,
    closure: &ClosureInfo,
    decl: &FunctionDecl,
) -> Result<(), Thrown> {
    let written = String::from_utf8_lossy(&decl.name.0).into_owned();
    if closure.fake {
        let named = match closure.scope {
            Some(scope) => format!("{}::{written}", req.class_name(scope)),
            None => written,
        };
        let named = PhpString::new(req, named.as_bytes())?.into_value();
        set_row(req, out, b"function", named)?;
    } else {
        let named = PhpString::new(req, written.as_bytes())?.into_value();
        set_row(req, out, b"name", named)?;
        let file = PhpString::new(req, &decl.file.0)?.into_value();
        set_row(req, out, b"file", file)?;
        set_row(req, out, b"line", Owner::int(i64::from(decl.line)))?;
    }
    // The whole table, the `use` variables and the body's `static`s.
    let bound = req.closure_bound(object, false);
    if !bound.is_empty() {
        let statics = PhpArray::filled(req, |req, statics| {
            for (name, value) in bound {
                let name = PhpString::new(req, &name)?;
                // `Z_ISREF_P(var) && Z_REFCOUNT_P(var) == 1`: a reference
                // the table alone holds shows its value.
                let value = if value.as_php_ref().is_some() && value.refcount() == 1 {
                    value.deref()
                } else {
                    value
                };
                let name = Key::from_string(req, name);
                statics.set(req, name, Owner::take(req, value))?;
            }
            Ok(())
        })?;
        set_row(req, out, b"static", statics.into_value())?;
    }
    if let Some(this) = closure.this {
        let this = Owner::take(req, this.as_value());
        set_row(req, out, b"this", this)?;
    }
    if !decl.parameters.is_empty() {
        let required = decl
            .parameters
            .iter()
            .position(|p| !p.is_required())
            .unwrap_or(decl.parameters.len());
        let parameters = PhpArray::filled(req, |req, parameters| {
            for (at, parameter) in decl.parameters.iter().enumerate() {
                let amp = if parameter.by_ref { "&" } else { "" };
                let name = format!("{amp}${}", String::from_utf8_lossy(&parameter.name.0));
                let stands = if at >= required {
                    &b"<optional>"[..]
                } else {
                    b"<required>"
                };
                let key = PhpString::new(req, name.as_bytes())?;
                let key = Key::from_string(req, key);
                let stands = PhpString::new(req, stands)?;
                parameters.set(req, key, stands.into_value())?;
            }
            Ok(())
        })?;
        set_row(req, out, b"parameter", parameters.into_value())?;
    }
    Ok(())
}

/// `zend_hash_update(debug_info, ZSTR_KNOWN(...), &val)`: one row of a
/// closure's table, its key a string of the request's that the array
/// takes and its value the array's from here.
fn set_row(
    req: &mut Request,
    table: &mut Owner<PhpArray>,
    name: &[u8],
    value: Owner<Value>,
) -> Result<(), Thrown> {
    let key = match PhpString::new(req, name) {
        Ok(key) => Key::from_string(req, key),
        Err(thrown) => {
            value.give_up(req);
            return Err(thrown);
        }
    };
    table.set(req, key, value)
}

static CLOSURE: ClosureHandlers = ClosureHandlers;

/// `zend_closure_clone`: the members copied and the payload's copy in
/// the new object's slot, with one more owner of the receiver taken
/// first, as `zend_create_closure` takes it.
fn closure_clone(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let Some(mut closure) = object.closure(req).cloned() else {
        return req.clone_members(object);
    };
    // `zend_array_dup` of the bound variables in
    // `zend_create_closure_ex`: the clone's table is its own and each
    // value in it one more owner, a reference shared with the original
    // as php shares one.
    closure.bound = closure
        .bound
        .map(|table| table.copy(req).map(Owner::hand_on))
        .transpose()?;
    if let Some(this) = closure.this {
        req.addref(&this.as_value());
    }
    let made = req.clone_with_payload(object, closure);
    adopted(made)
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
#[php_class(interface)]
pub(crate) struct UnitEnum;

#[php_class]
impl UnitEnum {
    fn cases() -> PhpArray;
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
#[php_class(interface, extends(UnitEnum))]
pub(crate) struct BackedEnum;

#[php_class]
impl BackedEnum {
    fn from(value: Union<i64, PhpString>) -> Static;
    fn tryFrom(value: Union<i64, PhpString>) -> Option<Static>;
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
#[php_class(dynamic_properties = allowed)]
pub(crate) struct stdClass;

#[php_class]
impl stdClass {}

/// `final class Closure` as `zend_register_closure_ce` declares it,
/// `ZEND_ACC_FINAL | ZEND_ACC_NO_DYNAMIC_PROPERTIES` and
/// `@not-serializable`, with the six methods `class_Closure_methods`
/// carries and no seventh: [`ClosureHandlers::get_method`] answers
/// `__invoke` as `zend_closure_get_method` does, off the table, the
/// trampoline [`trampolines`] lays down.
///
/// `__construct` is the first row of the stub's table. php reaches its
/// body through no lookup, since `zend_closure_get_constructor`
/// ([`ClosureHandlers::get_constructor`]) raises `Instantiation of class
/// Closure is not allowed` before `new` can find it; reflection reaches
/// it, and it raises the same.
///
/// ```php
/// try { (new ReflectionMethod('Closure', '__construct'))->invoke(fn () => 1, 'extra'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Instantiation of class Closure is not allowed
/// ```
#[php_class(
    final,
    not_serializable,
    dynamic_properties = forbidden,
    // `emalloc(sizeof(zend_closure))`, 344 bytes whole.
    size = 360,
    handlers = CLOSURE
)]
pub(crate) struct Closure;

#[php_class]
impl Closure {
    /// `ZEND_METHOD(Closure, __construct)`: php's `Error`, whatever it was
    /// handed, since php's body parses no parameter.
    #[php(private, arity = manual)]
    fn __construct(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let _ = this;
        Err(throw_error(
            req,
            "Error",
            "Instantiation of class Closure is not allowed",
        ))
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
    fn bind(
        req: &mut Request,
        closure: Instance<Closure>,
        newThis: Option<PhpObject>,
        #[php(default = "static")] newScope: Option<Union<PhpObject, PhpString>>,
    ) -> Result<Option<Owner<Instance<Closure>>>, Thrown> {
        adopted_some(do_closure_bind(
            req,
            closure.object(),
            newThis,
            NewScope::of(newScope),
        ))
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
    fn bindTo(
        req: &mut Request,
        this: This<Self>,
        newThis: Option<PhpObject>,
        #[php(default = "static")] newScope: Option<Union<PhpObject, PhpString>>,
    ) -> Result<Option<Owner<Instance<Closure>>>, Thrown> {
        adopted_some(do_closure_bind(
            req,
            this.object(),
            newThis,
            NewScope::of(newScope),
        ))
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
    ///
    /// Named arguments are placed at the closure's parameters:
    ///
    /// ```php
    /// class Foo {}
    /// $f = function ($a = 'a', $b = 'b', $c = 'c') { echo "$a $b $c\n"; };
    /// $f->call(new Foo, 'A', c: 'C');
    /// ```
    /// ```expect
    /// A b C
    /// ```
    ///
    /// A `static` closure takes no receiver:
    ///
    /// ```php
    /// class A {}
    /// var_dump((static function () { return 1; })->call(new A));
    /// ```
    /// ```expect
    /// Warning: Cannot bind an instance to a static closure, this will be an error in PHP 9 in %s on line 3
    /// NULL
    /// ```
    fn call(
        req: &mut Request,
        this: This<Self>,
        newThis: PhpObject,
        args: NamedVariadic<Value>,
    ) -> Result<Owner<Value>, Thrown> {
        let closure = this.object();
        let Some(info) = closure.closure(req).cloned() else {
            return Ok(Owner::null());
        };
        let class = newThis.class();
        if !valid_closure_binding(req, &info, Some(newThis), Some(class)) {
            return Ok(Owner::null());
        }
        // `fci.named_params`, placed by `zend_call_function` over the
        // closure's function.
        let placed = crate::api::call::placed(req, info.function, &args.positional, args.named())?;
        let returned = req.serve_closure_as(
            closure,
            newThis,
            class,
            &placed.arguments,
            &placed.extra_named,
        )?;
        // `zend_unwrap_reference(&closure_result)`: the cell a closure
        // returning by reference handed back given up for the value it
        // holds, whose owner is the caller's.
        let value = req.unwrap_reference(returned);
        Ok(Owner::adopt(value))
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
    /// A pair on an object shows the method's name with its class, the
    /// receiver and the parameters:
    ///
    /// ```php
    /// class C { function m($x, $y = 1) { return $x; } }
    /// var_dump(Closure::fromCallable([new C, 'm']));
    /// ```
    /// ```expect
    /// object(Closure)#2 (3) {
    ///   ["function"]=>
    ///   string(4) "C::m"
    ///   ["this"]=>
    ///   object(C)#1 (0) {
    ///   }
    ///   ["parameter"]=>
    ///   array(2) {
    ///     ["$x"]=>
    ///     string(10) "<required>"
    ///     ["$y"]=>
    ///     string(10) "<optional>"
    ///   }
    /// }
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
    fn fromCallable(
        req: &mut Request,
        callback: Raw<Callable>,
    ) -> Result<Owner<Instance<Closure>>, Thrown> {
        let given = callback.value();
        if let Some(object) = given.as_object().copied()
            && object.closure(req).is_some()
        {
            return Ok(Owner::take(req, object).into_instance());
        }
        // A trampoline a `get_closure` handler made has no scope, which
        // `zend_create_closure_from_callable` refuses with no reason.
        if let Some(object) = given.as_object().copied()
            && req.handler_closure(object).is_some()
        {
            return Err(throw_over_pending(
                req,
                "TypeError",
                "Failed to create closure from callable",
            ));
        }
        let callable = match req.callable_checked(&given) {
            Ok(_) if req.is_pending() => return Err(Thrown::of(req.take_pending_view())),
            Ok(callable) => callable,
            Err(Refusal::Unsaid) => {
                return Err(throw_over_pending(
                    req,
                    "TypeError",
                    "Failed to create closure from callable",
                ));
            }
            Err(refusal) => {
                return Err(refusal.across(req, |req| {
                    let message = format!("Failed to create closure from callable: {refusal}");
                    throw_over_pending(req, "TypeError", &message)
                }));
            }
        };
        let made = fake_closure(req, &callable)?;
        Ok(Owner::adopt(made).into_instance())
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
    fn getCurrent(req: &mut Request) -> Result<Instance<Closure>, Thrown> {
        let Some(closure) = req.calling_closure() else {
            return Err(throw_error(
                req,
                "Error",
                "Current function is not a closure",
            ));
        };
        Ok(Instance::of(closure))
    }
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
    let invoke = FunctionDecl {
        parameters: vec![variadic(b"args", Some(Type::Mixed))],
        return_type: Some(Type::Mixed),
        ..native_function(INVOKE)
    };
    vec![(
        Name(b"Closure".as_slice().into()),
        public_method(invoke, true),
        closure_invoke as Entry,
    )]
}

/// `zend_create_closure_from_callable` over what a callable resolved to:
/// [`closure_from_frame`] over the call frame `zend_is_callable_ex`
/// filled, a `Closure` as the frame's closure, a function with no scope,
/// and a method with the receiver it was taken with and the class the
/// callable named as its called scope.
pub(crate) fn fake_closure(req: &mut Request, callable: &Callable) -> Result<PhpObject, Thrown> {
    let frame = match callable.target() {
        Target::Closure(object) => CallFrame::Closure(object),
        Target::Function(function) => CallFrame::Callee {
            function,
            this: None,
            called_scope: None,
        },
        Target::Method {
            function,
            this,
            scope,
        } => CallFrame::Callee {
            function,
            this,
            called_scope: Some(scope),
        },
    };
    closure_from_frame(req, frame)
}

/// The call frame `zend_closure_from_frame` reads: one made through a
/// `Closure`, `ZEND_CALL_CLOSURE`, or any other callee with `EX(This)`,
/// the receiver where `ZEND_CALL_HAS_THIS` is set and the called scope,
/// which is the receiver's class where there is one. A view; no owner
/// is taken of any of them.
#[derive(Clone, Copy)]
pub(crate) enum CallFrame {
    Closure(PhpObject),
    Callee {
        function: Function,
        this: Option<PhpObject>,
        called_scope: Option<ClassId>,
    },
}

/// `zend_closure_from_frame` (`Z/Zend/zend_closures.c:878`), the one
/// closure made of a call frame, by `CALLABLE_CONVERT` and by
/// `Closure::fromCallable` alike: a call made through a `Closure` is
/// that closure, one more owner, and so is `__invoke` on one; every
/// other callee is `zend_create_fake_closure` over it, its scope the
/// class that declares it, bound to the frame's receiver, and its
/// called scope the receiver's class where it has one and the frame's
/// called scope where it has none.
pub(crate) fn closure_from_frame(req: &mut Request, frame: CallFrame) -> Result<PhpObject, Thrown> {
    let (function, this, called_scope) = match frame {
        CallFrame::Closure(closure) => {
            req.addref(&closure.as_value());
            return Ok(closure);
        }
        CallFrame::Callee {
            function,
            this,
            called_scope,
        } => (function, this, called_scope),
    };
    if let Some(this) = this
        && this.closure(req).is_some()
        && req
            .method_decl(function)
            .is_some_and(|decl| decl.function.name.0.eq_ignore_ascii_case(INVOKE))
    {
        req.addref(&this.as_value());
        return Ok(this);
    }
    // `ZEND_ACC_CALL_VIA_TRAMPOLINE`: `zend_closure_call_magic` in the
    // trampoline's place, of its name and its scope.
    let function = crate::trampoline::closure_call(req, function).unwrap_or(function);
    let scope = req.method_class(function);
    let fake = ClosureInfo {
        function,
        this,
        scope,
        called_scope,
        bound: None,
        fake: true,
    };
    create_closure(req, &fake, scope, called_scope, this)
}

/// `zend_ast_evaluate`'s `ZEND_AST_OP_ARRAY` arm: a closure of a
/// constant expression, function `at` of `unit`, made over the scope
/// that evaluates it as `zend_create_closure(result, func, scope, scope,
/// NULL)` makes it, with its own copy of the static variables the body
/// starts with.
///
/// ```php
/// class C {
///     const F = static function () { return static::class; };
///     public $p = static function () { static $n = 0; return ++$n; };
/// }
/// class D extends C {}
/// $c = new C;
/// var_dump((C::F)(), (D::F)(), ($c->p)(), ($c->p)(), ((new C)->p)());
/// function f($g = static function ($x) { return $x * 2; }) { return $g(21); }
/// var_dump(f());
/// ```
/// ```expect
/// string(1) "C"
/// string(1) "C"
/// int(1)
/// int(2)
/// int(3)
/// int(42)
/// ```
pub(crate) fn const_expr_closure(
    req: &mut Request,
    unit: &std::sync::Arc<php_unit::Unit>,
    at: u32,
    scope: Option<ClassId>,
) -> Result<PhpObject, Thrown> {
    let Some((_, body)) = unit.dynamic.get(at as usize) else {
        return Err(crate::api::raising::fatal(
            req,
            "InternalError: a constant expression's closure naming a function its unit has not",
        ));
    };
    let bound = crate::exec::starting_statics(req, body)?;
    let Some(function) = req.closure_function(unit, at) else {
        if let Some(bound) = bound {
            req.release(Value::array(bound));
        }
        return Err(crate::api::raising::fatal(
            req,
            "InternalError: a constant expression's closure naming a function its unit has not",
        ));
    };
    let info = ClosureInfo {
        function,
        this: None,
        scope,
        called_scope: scope,
        bound,
        fake: false,
    };
    let made = create_closure(req, &info, scope, scope, None);
    if let Some(bound) = info.bound {
        req.release(Value::array(bound));
    }
    made
}

/// `Closure::__invoke(mixed ...$args): mixed`, `ZEND_METHOD(Closure,
/// __invoke)` (`Z/Zend/zend_closures.c:45`): the payload's function
/// served with the frame's arguments, the extra named ones handed on
/// as `Z_PARAM_VARIADIC_WITH_NAMED` hands them, on the payload's
/// receiver where the closure is bound and as a function where it is
/// not (`zend_call_known_fcc`); what it returns is the call's.
pub(crate) fn closure_invoke(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    let Some(this) = req.this() else {
        return Err(throw_error(
            req,
            "Error",
            "Closure::__invoke called with no receiver",
        ));
    };
    let arguments: Vec<Value> = (1..=u32::try_from(req.argument_count()).unwrap_or(u32::MAX))
        .filter_map(|position| req.argument(position))
        .collect();
    let named = req.named_arguments();
    let served = req.serve_closure(this, &arguments, &named);
    adopted(served)
}

/// `interface Stringable`, `zend_interfaces.stub.php`'s: what
/// `zend_register_functions` adds to a class that declares `__toString`,
/// and what `Throwable` extends.
#[php_class(interface)]
pub(crate) struct Stringable;

#[php_class]
impl Stringable {
    fn __toString(this: This<Self>) -> PhpString;
}

/// `interface Throwable extends Stringable`, its seven methods in the
/// stub's order; `getCode` declares no return type, the stub's `@return
/// int` being a comment (its `TODO add proper type`).
#[php_class(interface, extends(Stringable))]
pub(crate) struct Throwable;

#[php_class]
impl Throwable {
    fn getMessage(this: This<Self>) -> PhpString;
    fn getCode(this: This<Self>) -> Untyped<Value>;
    fn getFile(this: This<Self>) -> PhpString;
    fn getLine(this: This<Self>) -> i64;
    fn getTrace(this: This<Self>) -> PhpArray;
    fn getPrevious(this: This<Self>) -> Option<Instance<Throwable>>;
    fn getTraceAsString(this: This<Self>) -> PhpString;
}

/// `interface ArrayAccess`, the four methods in the stub's order, each
/// parameter `mixed` as it writes them and each return tentative
/// (`@tentative-return-type`): a subclass that omits or narrows it is
/// php's deprecation and not a fatal.
#[php_class(interface)]
pub(crate) struct ArrayAccess;

#[php_class]
impl ArrayAccess {
    #[php(tentative)]
    fn offsetExists(this: This<Self>, offset: Value) -> bool;
    #[php(tentative)]
    fn offsetGet(this: This<Self>, offset: Value) -> Value;
    #[php(tentative)]
    fn offsetSet(this: This<Self>, offset: Value, value: Value);
    #[php(tentative)]
    fn offsetUnset(this: This<Self>, offset: Value);
}

/// `class Exception implements Throwable`, `zend_exceptions.stub.php`'s
/// seven properties and eleven methods in its order: the private
/// `__clone`, the constructor, `__wakeup` with its tentative `void`, the
/// seven final getters and `__toString`. Its objects are reached through
/// [`ThrowableHandlers`], which a user class extending it inherits, as
/// php's inherits `create_object`. `Throwable` is declared on it and on
/// `Error` alone, and the linker carries it down (design §6.2);
/// `Stringable` is never written, `Throwable` extending it.
///
/// ```php
/// $e = new Exception("m", 3, new Error("p"));
/// var_dump($e->getMessage(), $e->getCode(), $e->getFile() === __FILE__, $e->getLine(), $e->getTrace(), $e->getPrevious()->getMessage(), $e->getTraceAsString());
/// var_dump($e->__wakeup());
/// echo $e, "\n";
/// try { (new ReflectionMethod('Exception', '__clone'))->invoke($e, 'extra'); } catch (Exception $c) { echo $c->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "m"
/// int(3)
/// bool(true)
/// int(2)
/// array(0) {
/// }
/// string(1) "p"
/// string(9) "#0 {main}"
/// NULL
/// Error: p in %s:2
/// Stack trace:
/// #0 {main}
///
/// Next Exception: m in %s:2
/// Stack trace:
/// #0 {main}
/// Cannot clone object using __clone()
/// ```
#[php_class(implements(Throwable), handlers = THROWABLE)]
pub(crate) struct Exception;

#[php_class]
impl Exception {
    #[php(property, protected)]
    const message: Untyped<Value> = "";
    #[php(property, private)]
    const string: PhpString = "";
    #[php(property, protected)]
    const code: Untyped<Value> = 0;
    #[php(property, protected)]
    const file: PhpString = "";
    #[php(property, protected)]
    const line: i64 = 0;
    #[php(property, private)]
    const trace: PhpArray = ConstArray(&[]);
    #[php(property, private)]
    const previous: Option<Instance<Throwable>> = None;

    /// `ZEND_METHOD(Exception, __clone)`: private, so a call from outside
    /// the class never reaches it; what reflection reaches is php's
    /// `Exception` saying an object is not cloned this way, whatever it was
    /// handed, since php's body parses no parameter.
    #[php(private, arity = manual)]
    fn __clone(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let _ = this;
        Err(throw(
            req,
            "Exception",
            "Cannot clone object using __clone()",
        ))
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
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = "")] message: Passed<PhpString>,
        #[php(default = 0)] code: Passed<i64>,
        #[php(default = null)] previous: Passed<Option<Instance<Throwable>>>,
    ) -> Result<(), Thrown> {
        update_exception_properties(req, this.object(), message, code, previous)
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
    #[php(tentative)]
    fn __wakeup(this: This<Self>) {
        let _ = this;
    }

    /// `ZEND_METHOD(Exception, getMessage)`: the `message` property. The
    /// witness is the class's.
    #[php(final)]
    fn getMessage(req: &mut Request, this: This<Self>) -> Result<Raw<PhpString>, Thrown> {
        throwable_property(req, this.object(), b"message")
    }

    /// `ZEND_METHOD(Exception, getCode)`: the `code` property. The witness
    /// is the class's.
    #[php(final)]
    fn getCode(req: &mut Request, this: This<Self>) -> Result<Untyped<Value>, Thrown> {
        throwable_property::<Value>(req, this.object(), b"code").map(|code| Untyped(code.value()))
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
    #[php(final)]
    fn getFile(req: &mut Request, this: This<Self>) -> Result<Raw<PhpString>, Thrown> {
        throwable_property(req, this.object(), b"file")
    }

    /// `ZEND_METHOD(Exception, getLine)`: the `line` property. The witness
    /// is `getFile`'s.
    #[php(final)]
    fn getLine(req: &mut Request, this: This<Self>) -> Result<Raw<i64>, Thrown> {
        throwable_property(req, this.object(), b"line")
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
    #[php(final)]
    fn getTrace(req: &mut Request, this: This<Self>) -> Result<Owner<Raw<PhpArray>>, Thrown> {
        let trace = match this
            .object()
            .prop(req, b"trace")
            .filter(|value| value.as_array().is_some())
        {
            Some(trace) => Owner::take(req, trace),
            None => PhpArray::new(req)?.into_value(),
        };
        Ok(trace.into_raw())
    }

    /// `ZEND_METHOD(Exception, getPrevious)`: the `previous` property. The
    /// witness is the class's.
    #[php(final)]
    fn getPrevious(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Raw<Option<Instance<Throwable>>>, Thrown> {
        throwable_property(req, this.object(), b"previous")
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
    #[php(final)]
    fn getTraceAsString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let trace = req.trace_string_of(this.object());
        PhpString::new(req, trace.as_bytes())
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
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let made = req.exception_string(this.object());
        crate::api::values::adopted(made)
    }
}

/// `class ErrorException extends Exception`, `zend_exceptions.stub.php`'s:
/// one property of its own, `protected int $severity = E_ERROR`, after the
/// five it inherits, its constructor over php's six parameters and the
/// final `getSeverity`; the rest is `Exception`'s, inherited. What an
/// error handler converts a diagnostic into.
///
/// ```php
/// echo new ReflectionClass('ErrorException');
/// ```
/// ```expect
/// Class [ <internal:Core> class ErrorException extends Exception implements Throwable, Stringable ] {
///
///   - Constants [0] {
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [0] {
///   }
///
///   - Properties [5] {
///     Property [ protected $message = '' ]
///     Property [ protected $code = 0 ]
///     Property [ protected string $file = '' ]
///     Property [ protected int $line = 0 ]
///     Property [ protected int $severity = 1 ]
///   }
///
///   - Methods [11] {
///     Method [ <internal:Core, overwrites Exception, ctor> public method __construct ] {
///
///       - Parameters [6] {
///         Parameter #0 [ <optional> string $message = "" ]
///         Parameter #1 [ <optional> int $code = 0 ]
///         Parameter #2 [ <optional> int $severity = E_ERROR ]
///         Parameter #3 [ <optional> ?string $filename = null ]
///         Parameter #4 [ <optional> ?int $line = null ]
///         Parameter #5 [ <optional> ?Throwable $previous = null ]
///       }
///     }
///
///     Method [ <internal:Core> final public method getSeverity ] {
///
///       - Parameters [0] {
///       }
///       - Return [ int ]
///     }
///
///     Method [ <internal:Core, inherits Exception> public method __wakeup ] {
///
///       - Parameters [0] {
///       }
///       - Tentative return [ void ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getMessage ] {
///
///       - Parameters [0] {
///       }
///       - Return [ string ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getCode ] {
///
///       - Parameters [0] {
///       }
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getFile ] {
///
///       - Parameters [0] {
///       }
///       - Return [ string ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getLine ] {
///
///       - Parameters [0] {
///       }
///       - Return [ int ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getTrace ] {
///
///       - Parameters [0] {
///       }
///       - Return [ array ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getPrevious ] {
///
///       - Parameters [0] {
///       }
///       - Return [ ?Throwable ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getTraceAsString ] {
///
///       - Parameters [0] {
///       }
///       - Return [ string ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Stringable> public method __toString ] {
///
///       - Parameters [0] {
///       }
///       - Return [ string ]
///     }
///   }
/// }
/// ```
///
/// The handler that converts every diagnostic, the commonest there is:
/// the level becomes the severity and the diagnostic's site the
/// throwable's, and an uncaught one is rendered as any other.
///
/// ```php
/// set_error_handler(function (int $no, string $str, string $file, int $line) {
///     throw new ErrorException($str, 0, $no, $file, $line);
/// });
/// try { echo $undefined; } catch (ErrorException $e) {
///     var_dump($e->getMessage(), $e->getSeverity() === E_WARNING, $e->getLine(), $e->getFile() === __FILE__);
/// }
/// $a = [];
/// echo $a["missing"];
/// ```
/// ```expect exit=255
/// string(29) "Undefined variable $undefined"
/// bool(true)
/// int(5)
/// bool(true)
///
/// Fatal error: Uncaught ErrorException: Undefined array key "missing" in %s:9
/// Stack trace:
/// #0 %s(9): {closure:%s:2}(2, 'Undefined array...', '%s', 9)
/// #1 {main}
///   thrown in %s on line 9
/// ```
#[php_class(extends(Exception))]
pub(crate) struct ErrorException;

#[php_class]
impl ErrorException {
    #[php(property, protected)]
    const severity: i64 = Level::Error.bit();

    /// `ZEND_METHOD(ErrorException, __construct)`: `zend_parse_parameters`
    /// over `|SllS!l!O!`, then `Exception`'s three written as
    /// `zend_update_exception_properties` writes them, the severity
    /// always, the file where one was named, and the line where one was
    /// given, or `0` where a file was named without one.
    ///
    /// ```php
    /// $e = new ErrorException();
    /// var_dump($e->getSeverity(), $e->getLine(), $e->getFile() === __FILE__);
    /// $e = new ErrorException("m", 3, E_NOTICE, "f.php");
    /// var_dump($e->getMessage(), $e->getCode(), $e->getSeverity(), $e->getFile(), $e->getLine());
    /// $e = new ErrorException("m", 0, E_USER_ERROR, null, 42, new Exception("p"));
    /// var_dump($e->getLine(), $e->getFile() === __FILE__, $e->getPrevious()->getMessage());
    /// new ErrorException("m", 0, "high");
    /// ```
    /// ```expect exit=255
    /// int(1)
    /// int(2)
    /// bool(true)
    /// string(1) "m"
    /// int(3)
    /// int(8)
    /// string(5) "f.php"
    /// int(0)
    /// int(42)
    /// bool(true)
    /// string(1) "p"
    ///
    /// Fatal error: Uncaught TypeError: ErrorException::__construct(): Argument #3 ($severity) must be of type int, string given in %s:8
    /// Stack trace:
    /// #0 %s(8): ErrorException->__construct('m', 0, 'high')
    /// #1 {main}
    ///   thrown in %s on line 8
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = "")] message: Passed<PhpString>,
        #[php(default = 0)] code: Passed<i64>,
        #[php(default = E_ERROR)] severity: i64,
        #[php(default = null)] filename: Option<PhpString>,
        #[php(default = null)] line: Option<i64>,
        #[php(default = null)] previous: Passed<Option<Instance<Throwable>>>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        update_exception_properties(req, this, message, code, previous)?;
        let severity = Owner::int(severity);
        write_prop(req, &this, b"severity", severity)?;
        let named = filename.is_some();
        if let Some(filename) = filename {
            let filename = Owner::take(req, Value::string(filename));
            write_prop(req, &this, b"file", filename)?;
        }
        let line = match line {
            Some(line) => line,
            None if named => 0,
            None => return Ok(()),
        };
        let line = Owner::int(line);
        write_prop(req, &this, b"line", line)
    }

    /// `ZEND_METHOD(ErrorException, getSeverity)`: the `severity`
    /// property. The witnesses are the constructor's.
    #[php(final)]
    fn getSeverity(req: &mut Request, this: This<Self>) -> Result<Raw<i64>, Thrown> {
        throwable_property(req, this.object(), b"severity")
    }
}

/// `class Error implements Throwable`: `Exception`'s properties, `$line`
/// with no default, and its eleven methods, each the stub's
/// `@implementation-alias` of `Exception`'s, so one entry serves both.
#[php_class(implements(Throwable), handlers = THROWABLE)]
pub(crate) struct Error;

#[php_class]
impl Error {
    #[php(property, protected)]
    const message: Untyped<Value> = "";
    #[php(property, private)]
    const string: PhpString = "";
    #[php(property, protected)]
    const code: Untyped<Value> = 0;
    #[php(property, protected)]
    const file: PhpString = "";
    #[php(property, protected)]
    const line: i64;
    #[php(property, private)]
    const trace: PhpArray = ConstArray(&[]);
    #[php(property, private)]
    const previous: Option<Instance<Throwable>> = None;

    #[php(private, implementation = Exception::__clone)]
    fn __clone(this: This<Self>);
    #[php(implementation = Exception::__construct)]
    fn __construct(
        this: This<Self>,
        #[php(default = "")] message: Passed<PhpString>,
        #[php(default = 0)] code: Passed<i64>,
        #[php(default = null)] previous: Passed<Option<Instance<Throwable>>>,
    );
    #[php(tentative, implementation = Exception::__wakeup)]
    fn __wakeup(this: This<Self>);
    #[php(final, implementation = Exception::getMessage)]
    fn getMessage(this: This<Self>) -> Raw<PhpString>;
    #[php(final, implementation = Exception::getCode)]
    fn getCode(this: This<Self>) -> Untyped<Value>;
    #[php(final, implementation = Exception::getFile)]
    fn getFile(this: This<Self>) -> Raw<PhpString>;
    #[php(final, implementation = Exception::getLine)]
    fn getLine(this: This<Self>) -> Raw<i64>;
    #[php(final, implementation = Exception::getTrace)]
    fn getTrace(this: This<Self>) -> Raw<PhpArray>;
    #[php(final, implementation = Exception::getPrevious)]
    fn getPrevious(this: This<Self>) -> Raw<Option<Instance<Throwable>>>;
    #[php(final, implementation = Exception::getTraceAsString)]
    fn getTraceAsString(this: This<Self>) -> PhpString;
    #[php(implementation = Exception::__toString)]
    fn __toString(this: This<Self>) -> PhpString;
}

/// `class CompileError extends Error`.
#[php_class(extends(Error))]
pub(crate) struct CompileError;

#[php_class]
impl CompileError {}

/// `class ParseError extends CompileError`.
#[php_class(extends(CompileError))]
pub(crate) struct ParseError;

#[php_class]
impl ParseError {}

/// `class TypeError extends Error`.
#[php_class(extends(Error))]
pub(crate) struct TypeError;

#[php_class]
impl TypeError {}

/// `class ArgumentCountError extends TypeError`.
#[php_class(extends(TypeError))]
pub(crate) struct ArgumentCountError;

#[php_class]
impl ArgumentCountError {}

/// `class ValueError extends Error`.
#[php_class(extends(Error))]
pub(crate) struct ValueError;

#[php_class]
impl ValueError {}

/// `class ArithmeticError extends Error`.
#[php_class(extends(Error))]
pub(crate) struct ArithmeticError;

#[php_class]
impl ArithmeticError {}

/// `class DivisionByZeroError extends ArithmeticError`.
#[php_class(extends(ArithmeticError))]
pub(crate) struct DivisionByZeroError;

#[php_class]
impl DivisionByZeroError {}

/// `class UnhandledMatchError extends Error`.
#[php_class(extends(Error))]
pub(crate) struct UnhandledMatchError;

#[php_class]
impl UnhandledMatchError {}

/// `zend_ce_request_parse_body_exception`, the last class
/// `zend_register_default_exception` registers, which
/// `request_parse_body()` throws.
#[php_class(extends(Exception))]
pub(crate) struct RequestParseBodyException;

#[php_class]
impl RequestParseBodyException {}

/// `zend_update_exception_properties` (`Z/Zend/zend_exceptions.c:331`),
/// what `Exception::__construct` and `ErrorException::__construct` share:
/// each of `message`, `code` and `previous` that was given written onto
/// the receiver, the property's default left where one is absent.
fn update_exception_properties(
    req: &mut Request,
    this: PhpObject,
    message: Passed<PhpString>,
    code: Passed<i64>,
    previous: Passed<Option<Instance<Throwable>>>,
) -> Result<(), Thrown> {
    if let Passed::Given(message) = message {
        let message = Owner::take(req, Value::string(message));
        write_prop(req, &this, b"message", message)?;
    }
    if let Passed::Given(code) = code {
        let code = Owner::int(code);
        write_prop(req, &this, b"code", code)?;
    }
    if let Passed::Given(previous) = previous {
        let previous = previous.map_or_else(Value::null, |previous| previous.object().as_value());
        let previous = Owner::take(req, previous);
        write_prop(req, &this, b"previous", previous)?;
    }
    Ok(())
}

/// `GET_PROPERTY(ZEND_THIS, name)`: the getter's property read off the
/// receiver, `null` where the slot was never written, with one more
/// owner for the return, handed back as the slot holds it whatever the
/// getter declares, as `ZVAL_COPY` hands it back.
fn throwable_property<T: FromArgument>(
    req: &mut Request,
    this: PhpObject,
    name: &[u8],
) -> Result<Raw<T>, Thrown> {
    let value = this.prop(req, name).unwrap_or_default();
    Raw::from_value(req, value)
}

/// The `object|string|null $newScope` of `bind` and `bindTo`,
/// `Z_PARAM_OBJ_OR_STR_OR_NULL` with php's default `"static"`.
enum NewScope {
    Object(PhpObject),
    Named(Vec<u8>),
    Null,
}

impl NewScope {
    fn of(scope: Option<Union<PhpObject, PhpString>>) -> NewScope {
        match scope {
            Some(Union::A(object)) => NewScope::Object(object),
            Some(Union::B(name)) => NewScope::Named(name.as_bytes().to_vec()),
            None => NewScope::Null,
        }
    }
}

/// `do_closure_bind` (`Z/Zend/zend_closures.c:323`): the scope the
/// arguments name, an object's class, `"static"` the closure's own and
/// any other string the class of that name, php's warning and `null`
/// for one nothing declares; the binding held to
/// [`valid_closure_binding`]; then `zend_create_closure` over the
/// closure's function with the new receiver and scope, a receiver with
/// no scope taking `Closure` as its dummy scope as php's does.
///
/// The four spellings of the scope, through `bind` on a receiver and
/// `bindTo` on none: `"static"`, the closure's own, which for one
/// written at the top level is none, so a receiver takes `Closure`;
/// `null`, none; an object, its class; a name, the class it names. Then
/// the two refusals, a receiver for a `static` closure and the scope of
/// an internal class, each spelled through both.
///
/// ```php
/// class A {}
/// class B extends A {}
/// $get = function () {
///     $r = new ReflectionFunction(Closure::getCurrent());
///     return ($r->getClosureThis() ? 'A' : 'none') . ' in ' . $r->getClosureScopeClass()?->name;
/// };
/// foreach (['static', null, new B, 'A'] as $scope) {
///     var_dump(Closure::bind($get, new A, $scope)(), $get->bindTo(null, $scope)());
/// }
/// $static = static function () {};
/// var_dump(Closure::bind($static, new A), $static->bindTo(new A));
/// var_dump(Closure::bind($get, null, 'stdClass'), $get->bindTo(new A, new ArrayObject));
/// ```
/// ```expect
/// string(12) "A in Closure"
/// string(8) "none in "
/// string(12) "A in Closure"
/// string(8) "none in "
/// string(6) "A in B"
/// string(9) "none in B"
/// string(6) "A in A"
/// string(9) "none in A"
///
/// Warning: Cannot bind an instance to a static closure, this will be an error in PHP 9 in %s on line 12
///
/// Warning: Cannot bind an instance to a static closure, this will be an error in PHP 9 in %s on line 12
/// NULL
/// NULL
///
/// Warning: Cannot bind closure to scope of internal class stdClass, this will be an error in PHP 9 in %s on line 13
///
/// Warning: Cannot bind closure to scope of internal class ArrayObject, this will be an error in PHP 9 in %s on line 13
/// NULL
/// NULL
/// ```
fn do_closure_bind(
    req: &mut Request,
    closure: PhpObject,
    this: Option<PhpObject>,
    scope: NewScope,
) -> Result<Option<Instance<Closure>>, Thrown> {
    let Some(info) = closure.closure(req).cloned() else {
        return Ok(None);
    };
    let scope = match scope {
        NewScope::Object(object) => Some(object.class()),
        NewScope::Named(name) if name == b"static" => info.scope,
        NewScope::Named(name) => {
            let Some(class) = req.class_named(Bare::of(&name)) else {
                warning(
                    req,
                    &format!("Class \"{}\" not found", String::from_utf8_lossy(&name)),
                );
                return Ok(None);
            };
            Some(class)
        }
        NewScope::Null => None,
    };
    if !valid_closure_binding(req, &info, this, scope) {
        return Ok(None);
    }
    // `called_scope = newthis ? Z_OBJCE_P(newthis) : ce`, the new scope
    // before a receiver with none takes `Closure` as its dummy one.
    let called_scope = scope;
    let scope = scope.or_else(|| this.and_then(|_| req.class_named(Bare::resolved(b"Closure"))));
    create_closure(req, &info, scope, called_scope, this).map(|made| Some(Instance::of(made)))
}

/// `zend_create_closure` (`Z/Zend/zend_closures.c:855`) over the
/// function a closure holds: a new `Closure` with the given receiver
/// and scope, the receiver one more owner, and a copy of the variables
/// it bound, each one more owner, as `zend_array_dup` makes it. The
/// called scope is the receiver's class where there is one, else
/// `called_scope`, as `zend_create_closure_ex` stores it.
fn create_closure(
    req: &mut Request,
    info: &ClosureInfo,
    scope: Option<ClassId>,
    called_scope: Option<ClassId>,
    this: Option<PhpObject>,
) -> Result<PhpObject, Thrown> {
    let Some(class) = req.class_named(Bare::resolved(b"Closure")) else {
        return Err(throw_error(req, "Error", "Class \"Closure\" not found"));
    };
    let bound = info
        .bound
        .map(|table| table.copy(req).map(Owner::hand_on))
        .transpose()?;
    if let Some(this) = this {
        req.addref(&this.as_value());
    }
    req.new_object_with(
        class,
        ClosureInfo {
            function: info.function,
            this,
            scope,
            called_scope: this.map(|object| object.class()).or(called_scope),
            bound,
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
    let is_static = req.closure_refuses_receiver(info);
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
