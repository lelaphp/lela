//! The accessors `php_reflection.c` answers off one
//! `zend_class_entry`: the modifier bits, the parent, the interfaces
//! and traits, the function, property and constant tables, and the
//! three ways php makes an instance. Each reads the linked tables the
//! reflector's `$name` looks up again, the same `_class_string`
//! renders: the engine's function table (`Symbols::method_rows`,
//! `Symbols::method_row`), interface list (`Symbols::interfaces`) and
//! constants table (`Symbols::constants_in_order`), and
//! `linked_properties`.

use php_ext::abi::{Callable, Instance, NamedVariadic, OrFalse, Passed, Raw, This, Union};
use php_ext::attr::php_class;
use php_ext::call::{
    autoload, call_named, eval_declared, initialize_lazy, known_method, mark_lazy_initialized,
    read_class_prop_silent, update_class_constants, write_static_prop,
};
use php_ext::decl::{ClassKind, MethodDecl, Name, TraitAdaptation, Visibility};
use php_ext::raising::{Thrown, deprecated, throw_error, throw_exception_ex};
use php_ext::request::{MethodRow, Request};
use php_ext::values::{ClassId, Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef, walk};

use super::lazy::{Strategy, new_lazy, object_of, reset_as_lazy};
use super::{
    HeldMethod, Reference, answer, attr, class_chain, class_constant_named, class_extension_name,
    class_name_of, class_named, class_of, class_string, closure_invoke, constant_rows,
    constant_value, dynamic_properties, is_closure_invoke, is_internal, keep, keep_method,
    keep_reference, kept_closure, kept_object, linked_properties, lookup_class, method_or_invoke,
    namespace_end, new_object, property_named, reflection_class_factory, set_name, set_string_prop,
    string_value, this_class, user_line,
};

/// The name of the first of `traits` whose function table has `method`,
/// which `getTraitAliases` names for an alias that wrote no trait; the
/// link has refused a class where none does, so empty is unreachable.
fn trait_with(req: &mut Request, traits: &[Name], method: &[u8]) -> Result<Vec<u8>, Thrown> {
    for used in traits {
        let Some(at) = lookup_class(req, &used.0)? else {
            continue;
        };
        if req.symbols().method_row(at, method).is_some() {
            return Ok(class_name_of(req, at));
        }
    }
    Ok(Vec::new())
}

/// `ZEND_ACC_INTERFACE`, `ZEND_ACC_TRAIT` and `ZEND_ACC_ENUM`.
fn kind_of(req: &mut Request, class: ClassId) -> ClassKind {
    req.symbols()
        .class_decl(class)
        .map_or(ClassKind::Class, |d| d.kind)
}

/// `ZEND_ACC_EXPLICIT_ABSTRACT_CLASS | ZEND_ACC_IMPLICIT_ABSTRACT_CLASS`:
/// a class declared `abstract`, and one with an abstract method php
/// marks implicitly abstract at link time.
pub(super) fn class_is_abstract(req: &mut Request, class: ClassId) -> bool {
    let explicit = req
        .symbols()
        .class_decl(class)
        .is_some_and(|decl| decl.flags.is_abstract);
    explicit
        || req
            .symbols()
            .method_rows(class)
            .any(|row| req.symbols().row_method(row).is_some_and(|m| m.is_abstract))
}

/// php's `ce_flags & (INTERFACE | TRAIT | ABSTRACT | ENUM)` test, which
/// `isInstantiable` and `isCloneable` both open with.
fn cannot_instantiate(req: &mut Request, class: ClassId) -> bool {
    !matches!(kind_of(req, class), ClassKind::Class) || class_is_abstract(req, class)
}

/// `ce->constructor`: the linked `__construct`, with the row of the
/// function table it is.
fn constructor_of(req: &mut Request, class: ClassId) -> Option<(MethodDecl, MethodRow)> {
    let row = req.symbols().method_row(class, b"__construct")?;
    Some((req.symbols().row_method(row)?.clone(), row))
}

/// `reflection_method_factory(ce, mptr, ...)`: a `ReflectionMethod`
/// with `$name` the method's, `$class` the scope that declared it,
/// `intern->ce` the class it was reached through, which
/// `_function_string` renders against, so a method `B` inherits from
/// `A` reads `inherits A`, and `intern->ptr` the row of the function
/// table it is, none for a method no table holds.
///
/// ```php
/// class A { function m() {} } class B extends A {}
/// echo (new ReflectionClass('B'))->getMethod('m');
/// ```
/// ```expect
/// Method [ <user, inherits A> public method m ] {
///   @@ %s 2 - 2
/// }
/// ```
pub(super) fn method_factory(
    req: &mut Request,
    name: &[u8],
    scope: ClassId,
    ce: ClassId,
    held: Option<HeldMethod>,
) -> Result<Owner<PhpObject>, Thrown> {
    let class = class_named(req, b"ReflectionMethod")?;
    let object = new_object(req, class)?;
    let scope = class_name_of(req, scope);
    set_name(req, *object, name)?;
    set_string_prop(req, *object, b"class", &scope)?;
    keep(req, *object, ce, None);
    keep_method(req, *object, held);
    Ok(object)
}

/// `reflection_property_factory(ce, name, prop, ...)`: a
/// `ReflectionProperty` with `$name` the property's, `$class` the
/// class that declared it and `intern->ce` the class it was reached
/// through, which `getValue` holds its object to.
fn property_factory(
    req: &mut Request,
    name: &[u8],
    scope: ClassId,
    ce: ClassId,
) -> Result<Owner<PhpObject>, Thrown> {
    let class = class_named(req, b"ReflectionProperty")?;
    let object = new_object(req, class)?;
    let scope = class_name_of(req, scope);
    set_name(req, *object, name)?;
    set_string_prop(req, *object, b"class", &scope)?;
    keep(req, *object, ce, None);
    keep_reference(req, *object, Reference::Property)?;
    Ok(object)
}

/// `reflection_class_constant_factory`: a `ReflectionClassConstant`
/// with `$name` the constant's and `$class` the class that declared it.
fn class_constant_factory(
    req: &mut Request,
    name: &[u8],
    scope: ClassId,
) -> Result<Owner<PhpObject>, Thrown> {
    let class = class_named(req, b"ReflectionClassConstant")?;
    let object = new_object(req, class)?;
    let scope = class_name_of(req, scope);
    set_name(req, *object, name)?;
    set_string_prop(req, *object, b"class", &scope)?;
    Ok(object)
}

/// `Z_PARAM_OBJ_OF_CLASS_OR_STR(reflection_class_ptr)`'s value: a
/// `ReflectionClass` is the class it names, a string is
/// `zend_lookup_class`, and a miss is php's `ReflectionException`,
/// `Interface` or `Class` naming which it wanted.
fn reflector_class(
    req: &mut Request,
    given: Union<Instance<ReflectionClass>, PhpString>,
    what: &str,
) -> Result<ClassId, Thrown> {
    let name = match given {
        Union::A(reflector) => {
            let Some(name) = reflector
                .object()
                .prop(req, b"name")
                .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
            else {
                return Err(throw_error(
                    req,
                    "Error",
                    "Internal error: Failed to retrieve the argument's reflection object",
                ));
            };
            return class_named(req, &name);
        }
        Union::B(name) => name.as_bytes().to_vec(),
    };
    match autoload(req, &name)? {
        Some(class) => Ok(class),
        None => Err(throw_exception_ex(
            req,
            "ReflectionException",
            &format!(
                "{what} \"{}\" does not exist",
                String::from_utf8_lossy(&name)
            ),
        )),
    }
}

// -------------------------------------------------------- the tables

/// `_addmethod`'s filter: the default is
/// `ZEND_ACC_PPP_MASK | ABSTRACT | FINAL | STATIC`, which every method
/// carries at least one bit of, so `null` takes them all.
fn method_bits(decl: &MethodDecl) -> i64 {
    let mut bits = match decl.visibility {
        Visibility::Public => 1,
        Visibility::Protected => 2,
        Visibility::Private => 4,
    };
    if !decl.is_instance {
        bits |= 16;
    }
    if decl.is_final {
        bits |= 32;
    }
    if decl.is_abstract {
        bits |= 64;
    }
    bits
}

// ------------------------------------------------------ making one

/// The body `newInstance` and `newInstanceArgs` share: `object_init_ex`,
/// which refuses what cannot be instantiated in the engine's words;
/// then the object's own `get_constructor` asked under the class's fake
/// scope, so a private constructor is found and not refused as
/// `zend_std_get_constructor` refuses it from the caller's scope, and a
/// class whose handler refuses construction (`Generator`, `Closure`,
/// `Directory`) throws its own `Error`; then a non-public constructor
/// php's `Access to non-public constructor`, and a class with none
/// refusing arguments; else the constructor served with them, and the
/// object marked as one whose constructor failed where it throws
/// (`Z/ext/reflection/php_reflection.c:5072-5113`).
///
/// ```php
/// class Priv { private function __construct() {} } class None {} class Pub { function __construct(public $a = 0) {} }
/// foreach ([['Priv', []], ['None', [1]], ['None', []], ['Pub', [2]]] as $row) {
///     try { var_dump((new ReflectionClass($row[0]))->newInstanceArgs($row[1])); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// Access to non-public constructor of class Priv
/// Class None does not have a constructor, so you cannot pass any constructor arguments
/// object(None)#%d (0) {
/// }
/// object(Pub)#%d (1) {
///   ["a"]=>
///   int(2)
/// }
/// ```
///
/// ```php
/// trait T {}
/// foreach (['Closure', 'Generator', 'Directory', 'T'] as $c) {
///     try { (new ReflectionClass($c))->newInstance(); } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// Error: Instantiation of class Closure is not allowed
/// Error: The "Generator" class is reserved for internal use and cannot be manually instantiated
/// Error: Cannot directly construct Directory, use dir() instead
/// Error: Cannot instantiate trait T
/// ```
fn new_instance(
    req: &mut Request,
    class: ClassId,
    arguments: ConstructorArguments<'_>,
) -> Result<Owner<PhpObject>, Thrown> {
    let object = new_object(req, class)?;
    let asked = req.frames_mut().with_fake_scope(class, |req| {
        object.handlers(req).get_constructor(req, *object)
    });
    let constructor = match asked {
        Ok(constructor) => constructor.and_then(|_| constructor_of(req, class)),
        Err(thrown) => {
            object.give_up(req);
            return Err(thrown);
        }
    };
    let refusal = match &constructor {
        Some((decl, _)) if decl.visibility != Visibility::Public => {
            Some("Access to non-public constructor of class {}")
        }
        None if arguments.counted() > 0 => Some(
            "Class {} does not have a constructor, so you cannot pass any constructor arguments",
        ),
        None => return Ok(object),
        Some(_) => None,
    };
    if let Some(refusal) = refusal {
        object.give_up(req);
        let name = class_name_of(req, class);
        let message = refusal.replace("{}", &String::from_utf8_lossy(&name));
        return Err(throw_exception_ex(req, "ReflectionException", &message));
    }
    let Some(callable) = constructor
        .and_then(|(decl, _)| known_method(req, class, &decl.function.name, Some(*object)))
    else {
        return Ok(object);
    };
    let served = match arguments {
        ConstructorArguments::Variadic(args) => {
            call_named(req, &callable, &args.positional, args.named())
        }
        ConstructorArguments::Array(args) => Unpacked::of(req, args)
            .and_then(|unpacked| call_named(req, &callable, &unpacked.positional, &unpacked.named)),
    };
    match served {
        Ok(returned) => {
            returned.give_up(req);
            Ok(object)
        }
        Err(thrown) => {
            object.mark_destructor_called();
            object.give_up(req);
            Err(thrown)
        }
    }
}

/// What a constructor is handed by the two `newInstance` methods: the
/// call's own arguments, `Z_PARAM_VARIADIC_WITH_NAMED`, or an array php
/// passes as `named_params`, whose integer keys are positional and whose
/// string keys are named.
#[derive(Clone, Copy)]
enum ConstructorArguments<'a> {
    Variadic(&'a NamedVariadic<Value>),
    Array(PhpArray),
}

impl ConstructorArguments<'_> {
    /// What php's refusal for a class with no constructor counts:
    /// `ZEND_NUM_ARGS()`, the positional arguments alone, or
    /// `zend_hash_num_elements` of the array.
    fn counted(&self) -> usize {
        match self {
            ConstructorArguments::Variadic(args) => args.positional.len(),
            ConstructorArguments::Array(args) => args.len(),
        }
    }
}

/// An array as `zend_call_function` reads it for `named_params`: each
/// integer key an argument in order, each string key a named one.
struct Unpacked {
    positional: Vec<Value>,
    named: Vec<(PhpString, Value)>,
}

impl Unpacked {
    /// The array split, or php's `Error` for an integer key after a
    /// string one.
    fn of(req: &mut Request, args: PhpArray) -> Result<Unpacked, Thrown> {
        let mut unpacked = Unpacked {
            positional: Vec::new(),
            named: Vec::new(),
        };
        let mut misplaced = false;
        for (key, value) in walk::rows(req, args) {
            match key {
                Key::Str(name) => unpacked.named.push((name, value)),
                Key::Int(_) if !unpacked.named.is_empty() => {
                    misplaced = true;
                    break;
                }
                Key::Int(_) => unpacked.positional.push(value),
            }
        }
        if misplaced {
            return Err(throw_error(
                req,
                "Error",
                "Cannot use positional argument after named argument",
            ));
        }
        Ok(unpacked)
    }
}

/// `ReflectionClass` as the stub declares it.
///
/// `reflection_objects_new`, every reflector's `create_object`, allocates
/// a `reflection_object`, which `memory_get_usage()` counts (design
/// §5.13):
///
/// ```php
/// foreach ([false, true] as $shown) foreach (['ReflectionClass', 'ReflectionObject', 'ReflectionEnum', 'ReflectionFunction', 'ReflectionMethod', 'ReflectionProperty', 'ReflectionParameter', 'ReflectionClassConstant', 'ReflectionConstant', 'ReflectionNamedType', 'ReflectionUnionType', 'ReflectionExtension', 'ReflectionZendExtension'] as $c) {
///     $r = new ReflectionClass($c);
///     $m = memory_get_usage();
///     $o = $r->newInstanceWithoutConstructor();
///     $used = memory_get_usage() - $m;
///     $o = null;
///     if ($shown) echo $c, ' ', $used, "\n";
/// }
/// ```
/// ```expect
/// ReflectionClass 96
/// ReflectionObject 96
/// ReflectionEnum 96
/// ReflectionFunction 96
/// ReflectionMethod 112
/// ReflectionProperty 112
/// ReflectionParameter 96
/// ReflectionClassConstant 112
/// ReflectionConstant 96
/// ReflectionNamedType 80
/// ReflectionUnionType 80
/// ReflectionExtension 96
/// ReflectionZendExtension 96
/// ```
#[php_class(implements(crate::Reflector), not_serializable, size = 96, payload = crate::Reflected, handlers = crate::HANDLERS)]
pub(crate) struct ReflectionClass;

#[php_class]
impl ReflectionClass {
    const IS_IMPLICIT_ABSTRACT: i64 = 16;
    const IS_EXPLICIT_ABSTRACT: i64 = 64;
    const IS_FINAL: i64 = 32;
    const IS_READONLY: i64 = 65536;
    const SKIP_INITIALIZATION_ON_SERIALIZE: i64 = 8;
    const SKIP_DESTRUCTOR: i64 = 16;
    #[php(property)]
    const name: PhpString;

    #[php(private, implementation = crate::func::ReflectionFunctionAbstract::__clone)]
    fn __clone(this: This<Self>);

    /// `reflection_class_object_ctor` with `is_object` off,
    /// `ReflectionClass::__construct(object|string $objectOrClass)`: an
    /// object's class or the class named, its declared name kept as
    /// `$name`.
    ///
    /// ```php
    /// $r = new ReflectionClass('reflectionexception'); var_dump($r, (new ReflectionClass(new ReflectionException))->name);
    /// ```
    /// ```expect
    /// object(ReflectionClass)#1 (1) {
    ///   ["name"]=>
    ///   string(19) "ReflectionException"
    /// }
    /// string(19) "ReflectionException"
    /// ```
    ///
    /// ```php
    /// new ReflectionClass('Nope');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Class "Nope" does not exist in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClass->__construct('Nope')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// The `TypeError` for an array, `must be of type object|string, array
    /// given`, has no fence until the compiler reads an array expression.
    ///
    /// ```php
    /// new ReflectionClass(null);
    /// ```
    /// ```expect exit=255
    /// Deprecated: ReflectionClass::__construct(): Passing null to parameter #1 ($objectOrClass) of type object|string is deprecated in %s on line 2
    ///
    /// Fatal error: Uncaught ReflectionException: Class "" does not exist in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClass->__construct('')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// new ReflectionClass();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: ReflectionClass::__construct() expects exactly 1 argument, 0 given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClass->__construct()
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// A name is `zend_lookup_class`'s, so the autoloader is asked for a
    /// class not yet declared, once, and a miss throws after it.
    ///
    /// ```php
    /// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'Late') { class Late {} } });
    /// var_dump((new ReflectionClass('Late'))->name, (new ReflectionClass('\\Late'))->name);
    /// try { new ReflectionClass('Never'); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// load Late
    /// string(4) "Late"
    /// string(4) "Late"
    /// load Never
    /// Class "Never" does not exist
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        objectOrClass: Union<PhpObject, PhpString>,
    ) -> Result<(), Thrown> {
        let (class, _) = class_of(req, objectOrClass)?;
        let name = class_name_of(req, class);
        let this = this.object();
        set_name(req, this, &name)?;
        Ok(())
    }

    /// `ReflectionClass::__toString()`: `_class_string` over the class
    /// `$name` names, no object, indent empty. `JsonException` is the
    /// first class of the bundle with inherited members: `Exception`'s
    /// four protected properties, its three private ones shadowed and
    /// skipped, and ten of its eleven methods, the private `__clone`
    /// skipped, each `inherits Exception` with the prototype the interface
    /// gave it and `ctor` on the constructor.
    ///
    /// ```php
    /// echo new ReflectionClass('JsonException');
    /// ```
    /// ```expect
    /// Class [ <internal:json> class JsonException extends Exception implements Throwable, Stringable ] {
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
    ///   - Properties [4] {
    ///     Property [ protected $message = '' ]
    ///     Property [ protected $code = 0 ]
    ///     Property [ protected string $file = '' ]
    ///     Property [ protected int $line = 0 ]
    ///   }
    ///
    ///   - Methods [10] {
    ///     Method [ <internal:Core, inherits Exception, ctor> public method __construct ] {
    ///
    ///       - Parameters [3] {
    ///         Parameter #0 [ <optional> string $message = "" ]
    ///         Parameter #1 [ <optional> int $code = 0 ]
    ///         Parameter #2 [ <optional> ?Throwable $previous = null ]
    ///       }
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
    /// ```php
    /// echo new ReflectionClass('Throwable');
    /// ```
    /// ```expect
    /// Interface [ <internal:Core> interface Throwable extends Stringable ] {
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
    ///   - Properties [0] {
    ///   }
    ///
    ///   - Methods [8] {
    ///     Method [ <internal:Core> abstract public method getMessage ] {
    ///
    ///       - Parameters [0] {
    ///       }
    ///       - Return [ string ]
    ///     }
    ///
    ///     Method [ <internal:Core> abstract public method getCode ] {
    ///
    ///       - Parameters [0] {
    ///       }
    ///     }
    ///
    ///     Method [ <internal:Core> abstract public method getFile ] {
    ///
    ///       - Parameters [0] {
    ///       }
    ///       - Return [ string ]
    ///     }
    ///
    ///     Method [ <internal:Core> abstract public method getLine ] {
    ///
    ///       - Parameters [0] {
    ///       }
    ///       - Return [ int ]
    ///     }
    ///
    ///     Method [ <internal:Core> abstract public method getTrace ] {
    ///
    ///       - Parameters [0] {
    ///       }
    ///       - Return [ array ]
    ///     }
    ///
    ///     Method [ <internal:Core> abstract public method getPrevious ] {
    ///
    ///       - Parameters [0] {
    ///       }
    ///       - Return [ ?Throwable ]
    ///     }
    ///
    ///     Method [ <internal:Core> abstract public method getTraceAsString ] {
    ///
    ///       - Parameters [0] {
    ///       }
    ///       - Return [ string ]
    ///     }
    ///
    ///     Method [ <internal:Core, inherits Stringable> abstract public method __toString ] {
    ///
    ///       - Parameters [0] {
    ///       }
    ///       - Return [ string ]
    ///     }
    ///   }
    /// }
    /// ```
    ///
    /// A user class with a parent and an interface renders the same way,
    /// with no `getParentClass()` asked for it (#388): the header carries
    /// `extends P implements I`, the class's own constants and properties
    /// come before the ones it inherits, and an interface's constant is in
    /// the table too.
    ///
    /// ```php
    /// interface I { const J = 3; }
    /// class P { const A = 1; public $x = 1; }
    /// class C extends P implements I { const B = 2; public $y = 2; }
    /// echo new ReflectionClass('C');
    /// ```
    /// ```expect
    /// Class [ <user> class C extends P implements I ] {
    ///   @@ %s 4-4
    ///
    ///   - Constants [3] {
    ///     Constant [ public int B ] { 2 }
    ///     Constant [ public int A ] { 1 }
    ///     Constant [ public int J ] { 3 }
    ///   }
    ///
    ///   - Static properties [0] {
    ///   }
    ///
    ///   - Static methods [0] {
    ///   }
    ///
    ///   - Properties [2] {
    ///     Property [ public $y = 2 ]
    ///     Property [ public $x = 1 ]
    ///   }
    ///
    ///   - Methods [0] {
    ///   }
    /// }
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let class = this_class(req, this.object())?;
        // `_class_string(&str, ce, &intern->obj, "")`: the object reflector
        // hands the instance it kept, and the class reflector php's null
        // zval, which is what puts `Object of class` and the `Dynamic
        // properties` block on the one and not the other.
        let object = kept_object(req, this.object());
        let text = class_string(req, class, object, "")?;
        PhpString::new(req, text.as_bytes())
    }

    /// `ReflectionClass::getName()`: `ce->name`. `ReflectionObject` has no
    /// `getName` of its own and answers with this one, as php's inherited
    /// function table does.
    ///
    /// ```php
    /// class C {} var_dump((new ReflectionClass('c'))->getName(), (new ReflectionObject(new C))->getName());
    /// ```
    /// ```expect
    /// string(1) "C"
    /// string(1) "C"
    /// ```
    ///
    /// ```php
    /// ReflectionObject::getName();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Non-static method ReflectionClass::getName() cannot be called statically in %s:2
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// $r = new ReflectionClass('ReflectionException'); var_dump($r->getName(1));
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: ReflectionClass::getName() expects exactly 0 arguments, 1 given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClass->getName(1)
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    #[php(tentative)]
    fn getName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let class = this_class(req, this.object())?;
        let name = class_name_of(req, class);
        PhpString::declared(req, &name)
    }

    /// `ReflectionClass::isInternal()`: `ce->type == ZEND_INTERNAL_CLASS`,
    /// which for the declaration model is a class with no file.
    ///
    /// ```php
    /// class C {} var_dump((new ReflectionClass('C'))->isInternal(), (new ReflectionClass('ReflectionClass'))->isInternal(), (new ReflectionClass('C'))->isUserDefined(), (new ReflectionObject(new ReflectionProperty('Exception', 'message')))->isUserDefined());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isInternal(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this_class(req, this.object())?;
        Ok(is_internal(req, class))
    }

    /// `ReflectionClass::isUserDefined()`: `ce->type == ZEND_USER_CLASS`.
    ///
    /// ```php
    /// class C {}
    /// var_dump((new ReflectionClass('ArrayObject'))->isUserDefined(), (new ReflectionClass('C'))->isUserDefined());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isUserDefined(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this_class(req, this.object())?;
        Ok(!is_internal(req, class))
    }

    /// `ReflectionClass::isAnonymous()`: `ZEND_ACC_ANON_CLASS`, which a
    /// class declared by `new class` carries.
    ///
    /// ```php
    /// class Named {}
    /// var_dump((new ReflectionClass('Exception'))->isAnonymous(), (new ReflectionClass('Named'))->isAnonymous());
    /// $r = new ReflectionClass(new class extends Named {});
    /// var_dump($r->isAnonymous(), $r->getShortName() === $r->getName(), strstr($r->getName(), "\0", true));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// string(15) "Named@anonymous"
    /// ```
    #[php(tentative)]
    fn isAnonymous(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this_class(req, this.object())?;
        let anonymous = req
            .symbols()
            .class_decl(class)
            .is_some_and(|decl| decl.flags.is_anonymous);
        Ok(anonymous)
    }

    /// `ReflectionClass::isInstantiable()`: an interface, a trait, an enum
    /// or an abstract class is not; anything else is, unless its
    /// constructor is not public.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('Exception'))->isInstantiable());
    /// var_dump((new ReflectionClass('JsonSerializable'))->isInstantiable());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isInstantiable(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this_class(req, this.object())?;
        if cannot_instantiate(req, class) {
            return Ok(false);
        }
        let public = constructor_of(req, class)
            .is_none_or(|(decl, _)| decl.visibility == Visibility::Public);
        Ok(public)
    }

    /// `ReflectionClass::isCloneable()`: an interface, a trait, an enum or
    /// an abstract class is not; a class with a `__clone` is cloneable
    /// while that method is public; any other is while its objects'
    /// handlers have a `clone_obj`, read off the object a
    /// `ReflectionObject` kept, or off a fresh one php makes and gives up
    /// unconstructed. The handlers are the nearest internal class's, so
    /// the fresh object is one of that class, and a user class above no
    /// internal one has `Std`'s.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('Exception'))->isCloneable());
    /// var_dump((new ReflectionClass('ReflectionClass'))->isCloneable());
    /// class F extends SplFileObject { function __destruct() { echo "never\n"; } }
    /// var_dump((new ReflectionClass("SplFileObject"))->isCloneable(), (new ReflectionClass("F"))->isCloneable(), (new ReflectionClass("ArrayObject"))->isCloneable(), (new ReflectionObject(new SplTempFileObject))->isCloneable(), (new ReflectionObject(new SplFileInfo(__FILE__)))->isCloneable());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isCloneable(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this_class(req, this.object())?;
        if cannot_instantiate(req, class) {
            return Ok(false);
        }
        let clone = req
            .symbols()
            .method_row(class, b"__clone")
            .and_then(|row| req.symbols().row_method(row))
            .map(|clone| clone.visibility);
        if let Some(visibility) = clone {
            return Ok(visibility == Visibility::Public);
        }
        if let Some(object) = kept_object(req, this.object()) {
            return Ok(object.handlers(req).clone_obj().is_some());
        }
        let Some(internal) = class_chain(req, class)
            .into_iter()
            .find(|at| is_internal(req, *at))
        else {
            return Ok(true);
        };
        let fresh = new_object(req, internal)?;
        let cloneable = fresh.handlers(req).clone_obj().is_some();
        fresh.give_up(req);
        Ok(cloneable)
    }

    /// `ReflectionClass::getFileName()`: `ce->info.user.filename` for a
    /// user class, `false` for an internal one. The three site accessors
    /// share this witness; the name is the file's as the runner names
    /// it, a path under the oracle and `witness.php` here.
    ///
    /// ```php
    /// $r = new ReflectionClass('C'); var_dump($r->getFileName(), $r->getStartLine(), $r->getEndLine()); $r = new ReflectionClass('Exception'); var_dump($r->getFileName(), $r->getStartLine(), $r->getEndLine()); class C { public $a; }
    /// ```
    /// ```expect
    /// string(%d) "%s.php"
    /// int(2)
    /// int(2)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getFileName(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let class = this_class(req, this.object())?;
        let file = req
            .symbols()
            .class_decl(class)
            .filter(|decl| !decl.file.0.is_empty())
            .map(|decl| decl.file.0.to_vec());
        match file {
            Some(file) => PhpString::new(req, &file).map(OrFalse::Value),
            None => Ok(OrFalse::False),
        }
    }

    /// `ReflectionClass::getStartLine()`: `ce->info.user.line_start`, or
    /// `false` for an internal class.
    ///
    /// ```php
    /// class C
    /// {
    /// }
    /// $c = new ReflectionClass('C');
    /// $i = new ReflectionClass('ArrayObject');
    /// var_dump($c->getStartLine(), $i->getStartLine());
    /// ```
    /// ```expect
    /// int(2)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getStartLine(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        let class = this_class(req, this.object())?;
        Ok(user_line(req, class, |decl| decl.line))
    }

    /// `ReflectionClass::getEndLine()`: `ce->info.user.line_end`, the line
    /// of the closing brace, or `false` for an internal class.
    ///
    /// ```php
    /// class C
    /// {
    /// }
    /// var_dump((new ReflectionClass('C'))->getEndLine(), (new ReflectionClass('ArrayObject'))->getEndLine());
    /// ```
    /// ```expect
    /// int(4)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getEndLine(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        let class = this_class(req, this.object())?;
        Ok(user_line(req, class, |decl| decl.end_line))
    }

    /// `ReflectionClass::getDocComment()`: `ce->doc_comment`, and `false`
    /// where there is none.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('Exception'))->getDocComment());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getDocComment(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let class = this_class(req, this.object())?;
        let Some(doc) = req
            .symbols()
            .class_decl(class)
            .and_then(|d| d.doc_comment.clone())
        else {
            return Ok(OrFalse::False);
        };
        PhpString::new(req, &doc).map(OrFalse::Value)
    }

    /// `ReflectionClass::getConstructor()`:
    /// `reflection_method_factory` over `ce->constructor`, and `null`
    /// where the class has none.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('Exception'))->getConstructor()->getName());
    /// var_dump((new ReflectionClass('JsonSerializable'))->getConstructor());
    /// ```
    /// ```expect
    /// string(11) "__construct"
    /// NULL
    /// ```
    #[php(tentative)]
    fn getConstructor(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::func::ReflectionMethod>>>, Thrown> {
        let class = this_class(req, this.object())?;
        let Some((decl, row)) = constructor_of(req, class) else {
            return Ok(None);
        };
        let object = method_factory(
            req,
            &decl.function.name.0,
            row.scope(),
            class,
            Some(HeldMethod::Row(row)),
        )?;
        Ok(Some(object.into_instance()))
    }

    /// `ReflectionClass::hasMethod(string $name)`:
    /// `zend_hash_exists(&ce->function_table, lc_name)`.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('Exception'))->hasMethod('getCode'));
    /// var_dump((new ReflectionClass('Exception'))->hasMethod('nope'));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn hasMethod(req: &mut Request, this: This<Self>, name: PhpString) -> Result<bool, Thrown> {
        let name = name.as_bytes().to_vec();
        let class = this_class(req, this.object())?;
        if req.symbols().method_row(class, &name).is_some() {
            return Ok(true);
        }
        Ok(is_closure_invoke(req, class, &name.to_ascii_lowercase()))
    }

    /// `ReflectionClass::getMethod(string $name)`:
    /// `reflection_method_factory` over the function table's entry, and
    /// php's `ReflectionException` for a name it has not.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('JsonException'))->getMethod('getCode')->getName());
    /// ```
    /// ```expect
    /// string(7) "getCode"
    /// ```
    ///
    /// ```php
    /// (new ReflectionClass('Exception'))->getMethod('nope');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Method Exception::nope() does not exist in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClass->getMethod('nope')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    #[php(tentative)]
    fn getMethod(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
    ) -> Result<Owner<Instance<crate::func::ReflectionMethod>>, Thrown> {
        let name = name.as_bytes().to_vec();
        let class = this_class(req, this.object())?;
        let kept = kept_closure(req, this.object());
        let Some(found) = method_or_invoke(req, class, &name, kept) else {
            let named = class_name_of(req, class);
            return Err(throw_exception_ex(
                req,
                "ReflectionException",
                &format!(
                    "Method {}::{}() does not exist",
                    String::from_utf8_lossy(&named),
                    String::from_utf8_lossy(&name)
                ),
            ));
        };
        let object = method_factory(
            req,
            &found.decl.function.name.0,
            found.scope,
            class,
            found.held,
        )?;
        Ok(object.into_instance())
    }

    /// `ReflectionClass::getMethods(?int $filter = null)`: one
    /// `ReflectionMethod` per row of the function table whose flags the
    /// filter keeps, a parent's private method skipped as `_addmethod`
    /// skips it.
    ///
    /// ```php
    /// var_dump(count((new ReflectionClass('JsonSerializable'))->getMethods()));
    /// ```
    /// ```expect
    /// int(1)
    /// ```
    #[php(tentative)]
    fn getMethods(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] filter: Option<i64>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let filter = filter.unwrap_or(1 | 2 | 4 | 16 | 32 | 64);
        let class = this_class(req, this.object())?;
        let mut methods = PhpArray::new(req)?;
        // The rows the filter keeps, read before any object is made.
        let rows: Vec<(MethodRow, Name)> = req
            .symbols()
            .method_rows(class)
            .filter_map(|row| {
                let decl = req.symbols().row_method(row)?;
                let kept = (decl.visibility != Visibility::Private || row.scope() == class)
                    && method_bits(decl) & filter != 0;
                kept.then(|| (row, decl.function.name.clone()))
            })
            .collect();
        for (row, name) in rows {
            let object =
                method_factory(req, &name.0, row.scope(), class, Some(HeldMethod::Row(row)))?;
            methods.append(req, object.into_value())?;
        }
        // `zend_get_closure_invoke_method` over the kept closure, or a fresh
        // one, appended where the filter keeps a public instance method.
        let is_closure = lookup_class(req, b"Closure")?.is_some_and(|c| c == class);
        let kept = kept_closure(req, this.object())
            .and_then(|closure| closure.closure(req).map(|c| c.function));
        if is_closure
            && filter & 1 != 0
            && let Some(invoke) = closure_invoke(req, kept)
        {
            let object = method_factory(
                req,
                &invoke.decl.function.name.0,
                invoke.scope,
                class,
                invoke.held,
            )?;
            methods.append(req, object.into_value())?;
        }
        Ok(methods)
    }

    /// `ReflectionClass::hasProperty(string $name)`: the name in
    /// `ce->properties_info`, the class's own properties and every
    /// parent's, a parent's private one excluded as php's
    /// `property_info->ce == ce` test excludes it. php then asks a
    /// `ReflectionObject`'s instance for a dynamic property of that name;
    /// the reflector here keeps no instance, so that branch is `false`
    /// until it does.
    ///
    /// ```php
    /// class C { public $a; protected $b; private $c; public static $d; } $r = new ReflectionClass('C'); $e = new ReflectionClass('ReflectionException'); var_dump($r->hasProperty('a'), $r->hasProperty('b'), $r->hasProperty('c'), $r->hasProperty('d'), $r->hasProperty('e'), $r->hasProperty(1.5), $e->hasProperty('message'), $e->hasProperty('string'));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn hasProperty(req: &mut Request, this: This<Self>, name: PhpString) -> Result<bool, Thrown> {
        let name = name.as_bytes().to_vec();
        let class = this_class(req, this.object())?;
        if property_named(req, class, &name).is_some() {
            return Ok(true);
        }
        // `has_property(obj, name, ZEND_PROPERTY_EXISTS)` on the object a
        // `ReflectionObject` kept: its dynamic properties.
        let dynamic = match kept_object(req, this.object()) {
            Some(object) => dynamic_properties(req, class, object).contains(&name),
            None => false,
        };
        Ok(dynamic)
    }

    /// `ReflectionClass::getProperty(string $name)`:
    /// `reflection_property_factory` over the property info, and php's
    /// `ReflectionException` for a name the class has not.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('JsonException'))->getProperty('message'));
    /// ```
    /// ```expect
    /// object(ReflectionProperty)#2 (2) {
    ///   ["name"]=>
    ///   string(7) "message"
    ///   ["class"]=>
    ///   string(9) "Exception"
    /// }
    /// ```
    ///
    /// ```php
    /// (new ReflectionClass('Exception'))->getProperty('nope');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Property Exception::$nope does not exist in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClass->getProperty('nope')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// A `ReflectionObject` finds the object's dynamic properties too, with
    /// `$class` the object's class, and a `Base::name` spelling reaches the
    /// property through a class the reflected one extends:
    ///
    /// ```php
    /// class A { public $a = 1; } class B extends A {}
    /// $o = new B; $o->d = 2;
    /// var_dump((new ReflectionObject($o))->getProperty('d')->class, (new ReflectionClass('B'))->getProperty('A::a')->class);
    /// try { (new ReflectionClass('A'))->getProperty('B::a'); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Deprecated: Creation of dynamic property B::$d is deprecated in %s on line 3
    /// string(1) "B"
    /// string(1) "A"
    /// Fully qualified property name B::$a does not specify a base class of A
    /// ```
    #[php(tentative)]
    fn getProperty(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
    ) -> Result<Owner<Instance<crate::member::ReflectionProperty>>, Thrown> {
        let name = name.as_bytes().to_vec();
        let class = this_class(req, this.object())?;
        if let Some((scope, _)) = property_named(req, class, &name) {
            return property_factory(req, &name, scope, class).map(Owner::into_instance);
        }
        if let Some(object) = kept_object(req, this.object())
            && dynamic_properties(req, class, object).contains(&name)
        {
            return property_factory(req, &name, class, class).map(Owner::into_instance);
        }
        // `Base::name`: the property of a class the reflected one is, or
        // the exception below naming the part after the colons.
        let (mut ce, mut short) = (class, name.clone());
        if let Some(at) = name.windows(2).position(|pair| pair == b"::") {
            short = name[at + 2..].to_vec();
            let base = class_named(req, &name[..at])?;
            if !req.symbols().instance_of(class, base) {
                let (base, named) = (class_name_of(req, base), class_name_of(req, class));
                return Err(throw_exception_ex(
                    req,
                    "ReflectionException",
                    &format!(
                        "Fully qualified property name {}::${} does not specify a base class of {}",
                        String::from_utf8_lossy(&base),
                        String::from_utf8_lossy(&short),
                        String::from_utf8_lossy(&named)
                    ),
                ));
            }
            ce = base;
            if let Some((scope, _)) = property_named(req, ce, &short) {
                return property_factory(req, &short, scope, ce).map(Owner::into_instance);
            }
        }
        let named = class_name_of(req, ce);
        Err(throw_exception_ex(
            req,
            "ReflectionException",
            &format!(
                "Property {}::${} does not exist",
                String::from_utf8_lossy(&named),
                String::from_utf8_lossy(&short)
            ),
        ))
    }

    /// `ReflectionClass::getProperties(?int $filter = null)`: one
    /// `ReflectionProperty` per row of `ce->properties_info` whose flags
    /// the filter keeps, a parent's private property skipped as
    /// `_addproperty` skips it.
    ///
    /// ```php
    /// var_dump(count((new ReflectionClass('Exception'))->getProperties()));
    /// ```
    /// ```expect
    /// int(7)
    /// ```
    #[php(tentative)]
    fn getProperties(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] filter: Option<i64>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let filter = filter.unwrap_or(1 | 2 | 4 | 16);
        let class = this_class(req, this.object())?;
        let mut properties = PhpArray::new(req)?;
        for (decl, scope) in linked_properties(req, class) {
            if decl.visibility == Visibility::Private && scope != class {
                continue;
            }
            let bits = super::member::property_flags(req, scope, &decl.name.0, &decl);
            if bits & filter == 0 {
                continue;
            }
            let object = property_factory(req, &decl.name.0, scope, class)?;
            properties.append(req, object.into_value())?;
        }
        // `_adddynproperty` over the kept object's table, where the filter
        // keeps public ones: a dynamic property is public.
        if filter & 1 != 0
            && let Some(object) = kept_object(req, this.object())
        {
            for name in dynamic_properties(req, class, object) {
                let object = property_factory(req, &name, class, class)?;
                properties.append(req, object.into_value())?;
            }
        }
        Ok(properties)
    }

    /// `ReflectionClass::hasConstant(string $name)`:
    /// `zend_hash_exists(&ce->constants_table, name)`, the table php fills
    /// with the class's own constants and every parent's but the private
    /// ones (`do_inherit_class_constant`). A scalar is the string it
    /// coerces to, so `hasConstant(1)` asks for `"1"`.
    ///
    /// ```php
    /// class Foo { const c1 = 1; } $r = new ReflectionClass('Foo'); var_dump($r->hasConstant('c1'), $r->hasConstant('c2'), $r->hasConstant(1), (new ReflectionClass('ReflectionClass'))->hasConstant('IS_FINAL'));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn hasConstant(req: &mut Request, this: This<Self>, name: PhpString) -> Result<bool, Thrown> {
        let name = name.as_bytes().to_vec();
        let class = this_class(req, this.object())?;
        Ok(class_constant_named(req, class, &name).is_some())
    }

    /// `ReflectionClass::getConstants(?int $filter = null)`: the constants
    /// table as an array keyed by name, each value the constant
    /// expression evaluated in its declaring class.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('ReflectionFunction'))->getConstants());
    /// ```
    /// ```expect
    /// array(1) {
    ///   ["IS_DEPRECATED"]=>
    ///   int(2048)
    /// }
    /// ```
    #[php(tentative)]
    fn getConstants(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] filter: Option<i64>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let filter = filter.unwrap_or(1 | 2 | 4);
        let class = this_class(req, this.object())?;
        let mut constants = PhpArray::new(req)?;
        for (decl, scope) in constant_rows(req, class) {
            let bits = match decl.visibility {
                Visibility::Public => 1,
                Visibility::Protected => 2,
                Visibility::Private => 4,
            };
            if bits & filter == 0 {
                continue;
            }
            let value = match super::constant_value(req, &decl, scope) {
                Ok(value) => value,
                Err(thrown) => {
                    constants.give_up(req);
                    return Err(thrown);
                }
            };
            constants.set_str(req, &decl.name.0, value)?;
        }
        Ok(constants)
    }

    /// `ReflectionClass::getReflectionConstants(?int $filter = null)`: the
    /// same table, each row a `ReflectionClassConstant`.
    ///
    /// ```php
    /// var_dump(count((new ReflectionClass('ReflectionFunction'))->getReflectionConstants()));
    /// ```
    /// ```expect
    /// int(1)
    /// ```
    #[php(tentative)]
    fn getReflectionConstants(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] filter: Option<i64>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let filter = filter.unwrap_or(1 | 2 | 4);
        let class = this_class(req, this.object())?;
        let mut constants = PhpArray::new(req)?;
        for (decl, scope) in constant_rows(req, class) {
            let bits = match decl.visibility {
                Visibility::Public => 1,
                Visibility::Protected => 2,
                Visibility::Private => 4,
            };
            if bits & filter == 0 {
                continue;
            }
            let object = class_constant_factory(req, &decl.name.0, scope)?;
            constants.append(req, object.into_value())?;
        }
        Ok(constants)
    }

    ///
    /// ```php
    /// class C { const A = 1; }
    /// var_dump((new ReflectionClass('ReflectionClass'))->getConstant('IS_FINAL'), (new ReflectionClass('C'))->getConstant('A'));
    /// var_dump((new ReflectionClass('C'))->getConstant('NOPE'));
    /// ```
    /// ```expect
    /// int(32)
    /// int(1)
    ///
    /// Deprecated: ReflectionClass::getConstant() for a non-existent constant is deprecated, use ReflectionClass::hasConstant() to check if the constant exists in %s on line %d
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getConstant(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
    ) -> Result<Owner<Value>, Thrown> {
        let name = name.as_bytes().to_vec();
        let class = this_class(req, this.object())?;
        if let Some((declaring, constant)) = class_constant_named(req, class, &name) {
            return constant_value(req, &constant, declaring);
        }
        deprecated(
            req,
            "ReflectionClass::getConstant() for a non-existent constant is deprecated, use ReflectionClass::hasConstant() to check if the constant exists",
        );
        Ok(Owner::bool(false))
    }

    /// `ReflectionClass::getReflectionConstant(string $name)`: one
    /// `ReflectionClassConstant`, and `false` for a name the table has
    /// not.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('ReflectionFunction'))->getReflectionConstant('IS_DEPRECATED'));
    /// var_dump((new ReflectionClass('ReflectionFunction'))->getReflectionConstant('nope'));
    /// ```
    /// ```expect
    /// object(ReflectionClassConstant)#2 (2) {
    ///   ["name"]=>
    ///   string(13) "IS_DEPRECATED"
    ///   ["class"]=>
    ///   string(18) "ReflectionFunction"
    /// }
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getReflectionConstant(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
    ) -> Result<OrFalse<Owner<Instance<crate::member::ReflectionClassConstant>>>, Thrown> {
        let name = name.as_bytes().to_vec();
        let class = this_class(req, this.object())?;
        let Some((scope, decl)) = class_constant_named(req, class, &name) else {
            return Ok(OrFalse::False);
        };
        let object = class_constant_factory(req, &decl.name.0, scope)?;
        Ok(OrFalse::Value(object.into_instance()))
    }

    /// `ReflectionClass::getInterfaces()`: the same list, each name a
    /// `ReflectionClass`, keyed by the interface's name.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('JsonException'))->getInterfaces());
    /// ```
    /// ```expect
    /// array(2) {
    ///   ["Throwable"]=>
    ///   object(ReflectionClass)#2 (1) {
    ///     ["name"]=>
    ///     string(9) "Throwable"
    ///   }
    ///   ["Stringable"]=>
    ///   object(ReflectionClass)#3 (1) {
    ///     ["name"]=>
    ///     string(10) "Stringable"
    ///   }
    /// }
    /// ```
    #[php(tentative)]
    fn getInterfaces(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let class = this_class(req, this.object())?;
        let linked = req.symbols().interfaces(class).to_vec();
        if linked.is_empty() {
            return Ok(Owner::take(req, PhpArray::empty()));
        }
        let mut interfaces = PhpArray::new(req)?;
        for interface in linked {
            let name = class_name_of(req, interface);
            let object = reflection_class_factory(req, interface)?;
            interfaces.set_str(req, &name, object.into_value())?;
        }
        Ok(interfaces)
    }

    /// `ReflectionClass::getInterfaceNames()`: `ce->interfaces`, the
    /// linked list in php's order.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('JsonException'))->getInterfaceNames());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   string(9) "Throwable"
    ///   [1]=>
    ///   string(10) "Stringable"
    /// }
    /// ```
    ///
    /// The list is the linked record's (`Symbols::interfaces`), in php's
    /// order for a user class (the parent's, its own, then their
    /// parents) and for an internal one (the parent's, then each of its
    /// own followed by that one's parents).
    ///
    /// ```php
    /// interface I0 {} interface I1 extends I0 {} interface J {}
    /// class P implements J {} class C extends P implements I1, Countable { function count(): int { return 0; } function __toString(): string { return ''; } }
    /// echo implode(', ', (new ReflectionClass('C'))->getInterfaceNames()), "\n";
    /// echo implode(', ', (new ReflectionClass('ArrayIterator'))->getInterfaceNames()), "\n";
    /// echo implode(', ', (new ReflectionClass('RecursiveCachingIterator'))->getInterfaceNames()), "\n";
    /// var_dump((new ReflectionClass('C'))->implementsInterface('I0'), (new ReflectionClass('C'))->isSubclassOf('J'));
    /// ```
    /// ```expect
    /// J, I1, Countable, Stringable, I0
    /// SeekableIterator, Traversable, Iterator, ArrayAccess, Serializable, Countable
    /// Countable, ArrayAccess, OuterIterator, Traversable, Iterator, Stringable, RecursiveIterator
    /// bool(true)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn getInterfaceNames(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let class = this_class(req, this.object())?;
        let linked = req.symbols().interfaces(class).to_vec();
        if linked.is_empty() {
            return Ok(Owner::take(req, PhpArray::empty()));
        }
        let mut names = PhpArray::new(req)?;
        for interface in linked {
            let name = class_name_of(req, interface);
            let value = string_value(req, &name)?;
            names.append(req, value)?;
        }
        Ok(names)
    }

    /// `ReflectionClass::isInterface()`: `ZEND_ACC_INTERFACE`.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('JsonSerializable'))->isInterface());
    /// var_dump((new ReflectionClass('Exception'))->isInterface());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isInterface(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this_class(req, this.object())?;
        Ok(kind_of(req, class) == ClassKind::Interface)
    }

    /// `ReflectionClass::getTraits()`: the same list, each name a
    /// `ReflectionClass`, keyed by the trait's name.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('Exception'))->getTraits());
    /// ```
    /// ```expect
    /// array(0) {
    /// }
    /// ```
    #[php(tentative)]
    fn getTraits(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let class = this_class(req, this.object())?;
        let traits = req
            .symbols()
            .class_decl(class)
            .map(|decl| decl.traits.clone())
            .unwrap_or_default();
        if traits.is_empty() {
            return Ok(Owner::take(req, PhpArray::empty()));
        }
        let mut found = PhpArray::new(req)?;
        for trait_ in traits {
            let Some(at) = lookup_class(req, &trait_.0)? else {
                continue;
            };
            let name = class_name_of(req, at);
            let object = reflection_class_factory(req, at)?;
            found.set_str(req, &name, object.into_value())?;
        }
        Ok(found)
    }

    /// `ReflectionClass::getTraitNames()`: `ce->trait_names`, the traits
    /// the class itself uses.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('Exception'))->getTraitNames());
    /// ```
    /// ```expect
    /// array(0) {
    /// }
    /// ```
    #[php(tentative)]
    fn getTraitNames(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let class = this_class(req, this.object())?;
        let traits = req
            .symbols()
            .class_decl(class)
            .map(|decl| decl.traits.clone())
            .unwrap_or_default();
        if traits.is_empty() {
            return Ok(Owner::take(req, PhpArray::empty()));
        }
        let mut names = PhpArray::new(req)?;
        for trait_ in traits {
            let found = lookup_class(req, &trait_.0)?;
            let name = found.map_or_else(|| trait_.0.to_vec(), |found| class_name_of(req, found));
            let value = string_value(req, &name)?;
            names.append(req, value)?;
        }
        Ok(names)
    }

    /// `ReflectionClass::getTraitAliases()`: each `[T::]f as alias` the
    /// class's `use` lists wrote, in the order written, keyed by the
    /// alias, the value `T::f` with the trait as written, or for an `f`
    /// alone the first used trait whose table has it, and `f` as written.
    /// An `as` that only changes visibility names no alias and is left
    /// out, as is every `insteadof`.
    ///
    /// ```php
    /// namespace N;
    /// trait T1 { function m1() {} function m2() {} }
    /// trait T2 { function m3() {} }
    /// class C { use T1, T2 { T1::m1 as a1; M2 as protected a2; m3 as private; t2::M3 as a3; } }
    /// var_dump((new \ReflectionClass(C::class))->getTraitAliases(), (new \ReflectionClass('Exception'))->getTraitAliases());
    /// ```
    /// ```expect
    /// array(3) {
    ///   ["a1"]=>
    ///   string(8) "N\T1::m1"
    ///   ["a2"]=>
    ///   string(8) "N\T1::M2"
    ///   ["a3"]=>
    ///   string(8) "N\t2::M3"
    /// }
    /// array(0) {
    /// }
    /// ```
    #[php(tentative)]
    fn getTraitAliases(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let class = this_class(req, this.object())?;
        let (traits, adaptations) = req
            .symbols()
            .class_decl(class)
            .map(|decl| (decl.traits.clone(), decl.adaptations.clone()))
            .unwrap_or_default();
        // `ce->trait_aliases`, which every `as` of the class's `use`
        // lists makes, a visibility's alone among them.
        if !adaptations
            .iter()
            .any(|adaptation| matches!(adaptation, TraitAdaptation::As { .. }))
        {
            return Ok(Owner::take(req, PhpArray::empty()));
        }
        let mut aliases = PhpArray::new(req)?;
        for adaptation in adaptations {
            let TraitAdaptation::As {
                method,
                alias: Some(alias),
                ..
            } = adaptation
            else {
                continue;
            };
            let mut named = if let Some(written) = method.trait_name {
                written.0.to_vec()
            } else {
                trait_with(req, &traits, &method.method.0)?
            };
            named.extend_from_slice(b"::");
            named.extend_from_slice(&method.method.0);
            let value = string_value(req, &named)?;
            aliases.set_str(req, &alias.0, value)?;
        }
        Ok(aliases)
    }

    /// `ReflectionClass::isTrait()`: `ZEND_ACC_TRAIT`.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('Exception'))->isTrait());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isTrait(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this_class(req, this.object())?;
        Ok(kind_of(req, class) == ClassKind::Trait)
    }

    /// `ReflectionClass::isEnum()`: `ZEND_ACC_ENUM`.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('PropertyHookType'))->isEnum());
    /// var_dump((new ReflectionClass('Exception'))->isEnum());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    fn isEnum(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this_class(req, this.object())?;
        Ok(kind_of(req, class) == ClassKind::Enum)
    }

    /// `ReflectionClass::isAbstract()`: `ZEND_ACC_EXPLICIT_ABSTRACT_CLASS`
    /// or `ZEND_ACC_IMPLICIT_ABSTRACT_CLASS`, so an interface and a class
    /// with an abstract method both read `true`.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('ReflectionFunctionAbstract'))->isAbstract());
    /// var_dump((new ReflectionClass('Exception'))->isAbstract());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isAbstract(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = super::this_class(req, this.object())?;
        Ok(super::class::class_is_abstract(req, class))
    }

    /// `ReflectionClass::isFinal()`: `ZEND_ACC_FINAL`.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('ReflectionGenerator'))->isFinal());
    /// var_dump((new ReflectionClass('Exception'))->isFinal());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isFinal(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this_class(req, this.object())?;
        let final_ = req
            .symbols()
            .class_decl(class)
            .is_some_and(|decl| decl.flags.is_final);
        Ok(final_)
    }

    /// `ReflectionClass::isReadOnly()`: `ZEND_ACC_READONLY_CLASS`.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('Exception'))->isReadOnly());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    fn isReadOnly(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this_class(req, this.object())?;
        let readonly = req
            .symbols()
            .class_decl(class)
            .is_some_and(|decl| decl.flags.is_readonly);
        Ok(readonly)
    }

    /// `ReflectionClass::getModifiers()`: the `ZEND_ACC_FINAL`,
    /// `ZEND_ACC_EXPLICIT_ABSTRACT_CLASS` and `ZEND_ACC_READONLY_CLASS`
    /// bits of `ce_flags`, the numbers the stub's `IS_*` constants name.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('ReflectionGenerator'))->getModifiers());
    /// var_dump((new ReflectionClass('ReflectionFunctionAbstract'))->getModifiers());
    /// ```
    /// ```expect
    /// int(32)
    /// int(64)
    /// ```
    #[php(tentative)]
    fn getModifiers(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let class = this_class(req, this.object())?;
        let Some(flags) = req.symbols().class_decl(class).map(|decl| decl.flags) else {
            return Ok(0);
        };
        let mut bits = 0;
        if flags.is_final {
            bits |= 32;
        }
        if flags.is_abstract {
            bits |= 64;
        }
        if flags.is_readonly {
            bits |= 65536;
        }
        Ok(bits)
    }

    /// `ReflectionClass::isInstance(object $object)`:
    /// `instanceof_function` over the object's class.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('Exception'))->isInstance(new JsonException));
    /// var_dump((new ReflectionClass('JsonException'))->isInstance(new Exception));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isInstance(req: &mut Request, this: This<Self>, object: PhpObject) -> Result<bool, Thrown> {
        let of = object.class();
        let class = this_class(req, this.object())?;
        Ok(req.symbols().instance_of(of, class))
    }

    /// `ReflectionClass::newInstance(mixed ...$args)`: `object_init_ex`
    /// and the constructor served over the arguments, php's
    /// `ReflectionException` for a class that cannot be instantiated.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('JsonException'))->newInstance('x')->getMessage());
    /// ```
    /// ```expect
    /// string(1) "x"
    /// ```
    ///
    /// Named arguments reach the constructor as `Z_PARAM_VARIADIC_WITH_NAMED`
    /// hands them on, with php's `Error` for a name it does not declare; a
    /// class with no constructor counts only the positional ones.
    ///
    /// ```php
    /// class C { function __construct(public $a = 1, public $b = 2) {} }
    /// var_dump((new ReflectionClass('C'))->newInstance(b: 5));
    /// var_dump((new ReflectionClass('C'))->newInstance(7, b: 8)->a);
    /// try { (new ReflectionClass('C'))->newInstance(c: 1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { (new ReflectionClass('C'))->newInstance(1, a: 2); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump(get_class((new ReflectionClass('stdClass'))->newInstance(x: 1)));
    /// ```
    /// ```expect
    /// object(C)#2 (2) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["b"]=>
    ///   int(5)
    /// }
    /// int(7)
    /// Unknown named parameter $c
    /// Named parameter $a overwrites previous argument
    /// string(8) "stdClass"
    /// ```
    #[php(tentative)]
    fn newInstance(
        req: &mut Request,
        this: This<Self>,
        args: NamedVariadic<Value>,
    ) -> Result<Owner<PhpObject>, Thrown> {
        let class = this_class(req, this.object())?;
        new_instance(req, class, ConstructorArguments::Variadic(&args))
    }

    /// `ReflectionClass::newInstanceWithoutConstructor()`:
    /// `object_init_ex` alone, refused for an internal final class php
    /// gives a `create_object` handler, whose objects only its constructor
    /// may make whole. The object's inherited properties hold no default
    /// until the linker places them (#419), so a read of one is `null` here
    /// where php's is the declared default.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('JsonException'))->newInstanceWithoutConstructor() instanceof JsonException);
    /// enum Foo { case Bar; }
    /// try { (new ReflectionClass('Foo'))->newInstanceWithoutConstructor(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { (new ReflectionClass('Generator'))->newInstanceWithoutConstructor(); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// Cannot instantiate enum Foo
    /// Class Generator is an internal class marked as final that cannot be instantiated without invoking its constructor
    /// ```
    #[php(tentative)]
    fn newInstanceWithoutConstructor(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<PhpObject>, Thrown> {
        let class = this_class(req, this.object())?;
        let final_name = req
            .symbols()
            .class_decl(class)
            .filter(|decl| decl.flags.is_final)
            .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned());
        if let Some(name) = final_name
            && is_internal(req, class)
            && req.symbols().class_has_create_object(class)
        {
            return Err(throw_exception_ex(
                req,
                "ReflectionException",
                &format!(
                    "Class {name} is an internal class marked as final that cannot be instantiated without invoking its constructor"
                ),
            ));
        }
        // `object_init_ex`, whose own refusal of what cannot be instantiated
        // is the `Error` `new` raises, not a `ReflectionException`.
        let object = new_object(req, class)?;
        Ok(object)
    }

    /// `ReflectionClass::newInstanceArgs(array $args = [])`: the same,
    /// over the array.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('JsonException'))->newInstanceArgs(['x'])->getMessage());
    /// ```
    /// ```expect
    /// string(1) "x"
    /// ```
    ///
    /// The array is php's `named_params`: a string key names the
    /// parameter, and an integer key after one is refused.
    ///
    /// ```php
    /// class C { function __construct(public $a = 1, public $b = 2) {} }
    /// $r = new ReflectionClass('C');
    /// var_dump($r->newInstanceArgs(['b' => 5]), $r->newInstanceArgs([3, 'b' => 4])->b);
    /// try { $r->newInstanceArgs(['b' => 5, 6]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { (new ReflectionClass('stdClass'))->newInstanceArgs(['x' => 1]); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(C)#2 (2) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["b"]=>
    ///   int(5)
    /// }
    /// int(4)
    /// Cannot use positional argument after named argument
    /// Class stdClass does not have a constructor, so you cannot pass any constructor arguments
    /// ```
    ///
    /// What is no array is php's `TypeError`, whose tail names the value as
    /// `zend_zval_value_name` does and not as `zend_zval_type_name` does, so
    /// `true` stands where the type alone would read `bool` (#462):
    ///
    /// ```php
    /// (new ReflectionClass('JsonException'))->newInstanceArgs(true);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught TypeError: ReflectionClass::newInstanceArgs(): Argument #1 ($args) must be of type array, true given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClass->newInstanceArgs(true)
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    #[php(tentative)]
    fn newInstanceArgs(
        req: &mut Request,
        this: This<Self>,
        #[php(default = [])] args: PhpArray,
    ) -> Result<Option<Owner<PhpObject>>, Thrown> {
        let class = this_class(req, this.object())?;
        new_instance(req, class, ConstructorArguments::Array(args)).map(Some)
    }

    /// `ReflectionClass::newLazyGhost(callable $initializer, int $options = 0): object`:
    /// an object of the class whose properties wait for the initializer,
    /// served with the object in place the first time one is touched.
    ///
    /// ```php
    /// class C { public int $a; public $b = 2; public function __construct() { echo "init\n"; $this->a = 1; } }
    /// $o = (new ReflectionClass('C'))->newLazyGhost(function (C $o) { $o->__construct(); });
    /// var_dump($o);
    /// var_dump($o->a);
    /// var_dump($o);
    /// ```
    /// ```expect
    /// lazy ghost object(C)#3 (0) {
    ///   ["a"]=>
    ///   uninitialized(int)
    /// }
    /// init
    /// int(1)
    /// object(C)#3 (2) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["b"]=>
    ///   int(2)
    /// }
    /// ```
    fn newLazyGhost(
        req: &mut Request,
        this: This<Self>,
        initializer: Callable,
        #[php(default = 0)] options: i64,
    ) -> Result<Owner<PhpObject>, Thrown> {
        new_lazy(req, this.object(), Strategy::Ghost, &initializer, options)
    }

    /// `ReflectionClass::newLazyProxy(callable $factory, int $options = 0): object`:
    /// an object of the class whose accesses, once one is made, go to the
    /// instance the factory returns.
    ///
    /// ```php
    /// class C { public $a = 1; }
    /// $r = new ReflectionClass('C');
    /// $p = $r->newLazyProxy(function () { echo "factory\n"; $c = new C; $c->a = 5; return $c; });
    /// var_dump($p->a);
    /// $i = $r->initializeLazyObject($p);
    /// var_dump($i === $p, $i->a);
    /// $p->a = 7;
    /// var_dump($i->a, $p);
    /// ```
    /// ```expect
    /// factory
    /// int(5)
    /// bool(false)
    /// int(5)
    /// int(7)
    /// lazy proxy object(C)#3 (1) {
    ///   ["instance"]=>
    ///   object(C)#4 (1) {
    ///     ["a"]=>
    ///     int(7)
    ///   }
    /// }
    /// ```
    fn newLazyProxy(
        req: &mut Request,
        this: This<Self>,
        factory: Callable,
        #[php(default = 0)] options: i64,
    ) -> Result<Owner<PhpObject>, Thrown> {
        new_lazy(req, this.object(), Strategy::Proxy, &factory, options)
    }

    /// `ReflectionClass::resetAsLazyGhost(object $object, callable $initializer, int $options = 0): void`:
    /// the object's destructor served unless `SKIP_DESTRUCTOR`, its
    /// properties unset, and the object left a ghost of the class.
    ///
    /// ```php
    /// class C { public $a = 1; function __destruct() { echo "destruct\n"; } }
    /// $r = new ReflectionClass('C');
    /// $o = new C;
    /// $r->resetAsLazyGhost($o, function ($o) { echo "init\n"; $o->a = 3; });
    /// var_dump($r->isUninitializedLazyObject($o));
    /// try { $r->resetAsLazyGhost($o, fn () => null); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($o->a);
    /// $r->resetAsLazyGhost($o, function () {}, ReflectionClass::SKIP_DESTRUCTOR);
    /// echo "end\n";
    /// ```
    /// ```expect
    /// destruct
    /// bool(true)
    /// Object is already lazy
    /// init
    /// int(3)
    /// end
    /// ```
    fn resetAsLazyGhost(
        req: &mut Request,
        this: This<Self>,
        object: PhpObject,
        initializer: Callable,
        #[php(default = 0)] options: i64,
    ) -> Result<(), Thrown> {
        reset_as_lazy(
            req,
            this.object(),
            Strategy::Ghost,
            object,
            &initializer,
            options,
        )
    }

    /// `ReflectionClass::resetAsLazyProxy(object $object, callable $factory, int $options = 0): void`:
    /// the same reset, the object left a proxy.
    ///
    /// ```php
    /// class C { public $a = 1; }
    /// $r = new ReflectionClass('C');
    /// $o = new C;
    /// $r->resetAsLazyProxy($o, function ($o) { echo "factory\n"; return new C; });
    /// var_dump($o);
    /// $o->a = 2;
    /// var_dump($o);
    /// ```
    /// ```expect
    /// lazy proxy object(C)#2 (0) {
    /// }
    /// factory
    /// lazy proxy object(C)#2 (1) {
    ///   ["instance"]=>
    ///   object(C)#4 (1) {
    ///     ["a"]=>
    ///     int(2)
    ///   }
    /// }
    /// ```
    fn resetAsLazyProxy(
        req: &mut Request,
        this: This<Self>,
        object: PhpObject,
        factory: Callable,
        #[php(default = 0)] options: i64,
    ) -> Result<(), Thrown> {
        reset_as_lazy(
            req,
            this.object(),
            Strategy::Proxy,
            object,
            &factory,
            options,
        )
    }

    /// `ReflectionClass::initializeLazyObject(object $object): object`: a
    /// lazy object initialised now, and the object its properties live on,
    /// the ghost itself or the proxy's instance.
    ///
    /// ```php
    /// class C { public $a = 1; public int $b; }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function ($o) { $o->a = 2; $o->b = 3; $o->dyn = 4; throw new Exception('no'); });
    /// try { $r->initializeLazyObject($o); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($r->isUninitializedLazyObject($o), $o);
    /// ```
    /// ```expect
    ///
    /// Deprecated: Creation of dynamic property C::$dyn is deprecated in %s on line 4
    /// no
    /// bool(true)
    /// lazy ghost object(C)#3 (0) {
    ///   ["b"]=>
    ///   uninitialized(int)
    /// }
    /// ```
    fn initializeLazyObject(
        req: &mut Request,
        this: This<Self>,
        object: PhpObject,
    ) -> Result<PhpObject, Thrown> {
        let class = this_class(req, this.object())?;
        let object = object_of(req, object, class, 1)?;
        initialize_lazy(req, &object)
    }

    /// `ReflectionClass::isUninitializedLazyObject(object $object): bool`.
    ///
    /// ```php
    /// class C { public $a = 1; }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function () {});
    /// var_dump($r->isUninitializedLazyObject($o), $r->isUninitializedLazyObject(new C));
    /// $o->a;
    /// var_dump($r->isUninitializedLazyObject($o));
    /// try { $r->isUninitializedLazyObject(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// ReflectionClass::isUninitializedLazyObject(): Argument #1 ($object) must be of type C, stdClass given
    /// ```
    fn isUninitializedLazyObject(
        req: &mut Request,
        this: This<Self>,
        object: PhpObject,
    ) -> Result<bool, Thrown> {
        let class = this_class(req, this.object())?;
        let object = object_of(req, object, class, 1)?;
        Ok(object.is_lazy())
    }

    /// `ReflectionClass::markLazyObjectAsInitialized(object $object): object`:
    /// each waiting property takes its default, the initializer is never
    /// served, and the answer is `initializeLazyObject`'s.
    ///
    /// ```php
    /// class C { public $a = 1; public int $b; public $c; }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function () { echo "never\n"; });
    /// $r->getProperty('c')->setRawValueWithoutLazyInitialization($o, 3);
    /// var_dump($r->markLazyObjectAsInitialized($o) === $o, $o);
    /// ```
    /// ```expect
    /// bool(true)
    /// object(C)#3 (2) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["b"]=>
    ///   uninitialized(int)
    ///   ["c"]=>
    ///   int(3)
    /// }
    /// ```
    fn markLazyObjectAsInitialized(
        req: &mut Request,
        this: This<Self>,
        object: PhpObject,
    ) -> Result<PhpObject, Thrown> {
        let class = this_class(req, this.object())?;
        let object = object_of(req, object, class, 1)?;
        mark_lazy_initialized(req, &object)
    }

    /// `ReflectionClass::getLazyInitializer(object $object): ?callable`:
    /// the initializer as it was given, while nothing has initialised the
    /// object, and `null` otherwise.
    ///
    /// ```php
    /// class C { public $a = 1; }
    /// function init($o) {}
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost('init');
    /// var_dump($r->getLazyInitializer($o));
    /// $r->initializeLazyObject($o);
    /// var_dump($r->getLazyInitializer($o), $r->getLazyInitializer(new C));
    /// ```
    /// ```expect
    /// string(4) "init"
    /// NULL
    /// NULL
    /// ```
    fn getLazyInitializer(
        req: &mut Request,
        this: This<Self>,
        object: PhpObject,
    ) -> Result<Raw<Option<Callable>>, Thrown> {
        let class = this_class(req, this.object())?;
        let object = object_of(req, object, class, 1)?;
        let Some(initializer) = object.lazy_initializer(req) else {
            return answer(Ok(Value::null()), req);
        };
        // php's `RETURN_COPY` of the zval it stored: the initializer is
        // held as the value it was given, which `Callable` would resolve
        // again rather than hand back; the engine takes its owner.
        answer(Ok(initializer), req)
    }

    /// `ReflectionClass::getParentClass()`:
    /// `zend_reflection_class_factory` over `ce->parent`, and `false`
    /// where the class extends nothing.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('JsonException'))->getParentClass()->getName());
    /// var_dump((new ReflectionClass('Exception'))->getParentClass());
    /// ```
    /// ```expect
    /// string(9) "Exception"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getParentClass(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<Instance<ReflectionClass>>>, Thrown> {
        let class = this_class(req, this.object())?;
        let Some(parent) = class_chain(req, class).get(1).copied() else {
            return Ok(OrFalse::False);
        };
        let object = reflection_class_factory(req, parent)?;
        Ok(OrFalse::Value(object.into_instance()))
    }

    /// `ReflectionClass::isSubclassOf(ReflectionClass|string $class)`:
    /// `ce != class_ce && instanceof_function(ce, class_ce)`, so a class
    /// is not a subclass of itself and is one of every parent and every
    /// interface up the chain. A `ReflectionClass` argument is the class
    /// it reflects; a string is looked up, a miss php's `ReflectionException`.
    ///
    /// ```php
    /// class A {} $a = new ReflectionClass('A'); $e = new ReflectionClass('ReflectionException'); var_dump($a->isSubclassOf($a), $e->isSubclassOf('exception'), $e->isSubclassOf(new ReflectionClass('Throwable')), $e->isSubclassOf('Stringable'), $e->isSubclassOf('ReflectionException'), $a->isSubclassOf('ReflectionException'));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// ```
    ///
    /// ```php
    /// class A {} (new ReflectionClass('A'))->isSubclassOf('X');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Class "X" does not exist in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClass->isSubclassOf('X')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    /// The `TypeError` for an array has no fence until the compiler reads
    /// an array expression.
    #[php(tentative)]
    fn isSubclassOf(
        req: &mut Request,
        this: This<Self>,
        class: Union<Instance<ReflectionClass>, PhpString>,
    ) -> Result<bool, Thrown> {
        let this = this_class(req, this.object())?;
        let of = reflector_class(req, class, "Class")?;
        Ok(this != of && req.symbols().instance_of(this, of))
    }

    /// `ReflectionClass::getStaticProperties()`: `CE_STATIC_MEMBERS(ce)` as
    /// an array, one slot per `static` property of `properties_info` under
    /// its own name, in that table's order, the slots laid down from their
    /// defaults on first use (`zend_class_init_statics`). php skips a
    /// property a class above declared private, one declared as an
    /// instance property, and a typed one nothing has written, whose slot
    /// stands `IS_UNDEF`; each value is handed back deref'd, with one more
    /// owner, which is what "enforce read only access" comes to. The
    /// class is updated first (`zend_update_class_constants`), so a
    /// constant or a default that does not evaluate is the throwable it
    /// raised, whether or not a static property reads it.
    ///
    /// ```php
    /// class Foo { const int CONST1 = C; public int $prop1 = Foo::CONST1; }
    /// define("C", "bar");
    /// try { (new ReflectionClass(Foo::class))->getStaticProperties(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot assign string to class constant Foo::CONST1 of type int
    /// ```
    ///
    /// ```php
    /// class A { public static int $n; public static $p = 1; private static $q = 2; protected static $r = 3; public $i = 4; }
    /// class B extends A { private static $q = 5; }
    /// var_dump((new ReflectionClass('A'))->getStaticProperties(), (new ReflectionClass('B'))->getStaticProperties());
    /// ```
    /// ```expect
    /// array(3) {
    ///   ["p"]=>
    ///   int(1)
    ///   ["q"]=>
    ///   int(2)
    ///   ["r"]=>
    ///   int(3)
    /// }
    /// array(3) {
    ///   ["q"]=>
    ///   int(5)
    ///   ["p"]=>
    ///   int(1)
    ///   ["r"]=>
    ///   int(3)
    /// }
    /// ```
    ///
    /// ```php
    /// (new ReflectionClass('Exception'))->getStaticProperties(1);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: ReflectionClass::getStaticProperties() expects exactly 0 arguments, 1 given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClass->getStaticProperties(1)
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    #[php(tentative)]
    fn getStaticProperties(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let class = this_class(req, this.object())?;
        update_class_constants(req, class)?;
        let mut properties = PhpArray::new(req)?;
        for (property, declaring) in linked_properties(req, class) {
            if property.is_instance
                || (property.visibility == Visibility::Private && declaring != class)
            {
                continue;
            }
            let value = match read_class_prop_silent(req, class, &property.name.0) {
                Ok(Some(value)) => value,
                Ok(None) => continue,
                Err(thrown) => {
                    properties.give_up(req);
                    return Err(thrown);
                }
            };
            if matches!(value.as_ref(), ValueRef::Undef) {
                value.give_up(req);
                continue;
            }
            properties.set_str(req, &property.name.0, value)?;
        }
        Ok(properties)
    }

    /// `ReflectionClass::getStaticPropertyValue(string $name, mixed
    /// $default = UNKNOWN)`: the slot addressed under `BP_VAR_IS` with the
    /// fake scope set to the class ([`read_class_prop_silent`]), so a
    /// property the class itself declared private is reached and one a
    /// class above did is not. A slot holding a value is that value, one
    /// more owner taken (`RETURN_COPY_DEREF`); otherwise `$default` where
    /// the call gave one, which php hands back before it looks at why the
    /// slot was empty; otherwise php's `Error` for a typed property
    /// nothing has written, whose text here is `Typed property` and not
    /// the engine's `Typed static property`, and php's
    /// `ReflectionException` for a name the class does not have.
    ///
    /// ```php
    /// class A { private static $q = 'a'; protected static $r = 'b'; public static $p = 'c'; }
    /// class B extends A { private static $q = 'd'; }
    /// $a = new ReflectionClass('A'); $b = new ReflectionClass('B');
    /// var_dump($a->getStaticPropertyValue('q'), $a->getStaticPropertyValue('r'), $a->getStaticPropertyValue('p'));
    /// var_dump($b->getStaticPropertyValue('q'), $b->getStaticPropertyValue('r'), $b->getStaticPropertyValue('nope', 'default value'));
    /// ```
    /// ```expect
    /// string(1) "a"
    /// string(1) "b"
    /// string(1) "c"
    /// string(1) "d"
    /// string(1) "b"
    /// string(13) "default value"
    /// ```
    ///
    /// A typed property nothing has written is the `Error`, and a default
    /// stands before it.
    ///
    /// ```php
    /// class A { public static int $n; }
    /// var_dump((new ReflectionClass('A'))->getStaticPropertyValue('n', 'd'));
    /// (new ReflectionClass('A'))->getStaticPropertyValue('n');
    /// ```
    /// ```expect exit=255
    /// string(1) "d"
    ///
    /// Fatal error: Uncaught Error: Typed property A::$n must not be accessed before initialization in %s:4
    /// Stack trace:
    /// #0 %s(4): ReflectionClass->getStaticPropertyValue('n')
    /// #1 {main}
    ///   thrown in %s on line 4
    /// ```
    ///
    /// An instance property is a name the class does not have, as php's
    /// `BP_VAR_IS` hands back nothing for one.
    ///
    /// ```php
    /// class A { public $i = 1; }
    /// (new ReflectionClass('A'))->getStaticPropertyValue('i');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Property A::$i does not exist in %s:3
    /// Stack trace:
    /// #0 %s(3): ReflectionClass->getStaticPropertyValue('i')
    /// #1 {main}
    ///   thrown in %s on line 3
    /// ```
    ///
    /// ```php
    /// (new ReflectionClass('Exception'))->getStaticPropertyValue();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: ReflectionClass::getStaticPropertyValue() expects at least 1 argument, 0 given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClass->getStaticPropertyValue()
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    #[php(tentative)]
    fn getStaticPropertyValue(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        default: Passed<Value>,
    ) -> Result<Owner<Value>, Thrown> {
        let name = name.as_bytes().to_vec();
        let class = this_class(req, this.object())?;
        update_class_constants(req, class)?;
        let held = match read_class_prop_silent(req, class, &name)? {
            Some(value) if !matches!(value.as_ref(), ValueRef::Undef) => return Ok(value),
            Some(value) => {
                value.give_up(req);
                true
            }
            None => false,
        };
        if let Passed::Given(default) = default {
            return Ok(Owner::take(req, default));
        }
        let shown = class_name_of(req, class);
        let shown = String::from_utf8_lossy(&shown).into_owned();
        let name = String::from_utf8_lossy(&name).into_owned();
        if held {
            return Err(throw_error(
                req,
                "Error",
                &format!(
                    "Typed property {shown}::${name} must not be accessed before initialization"
                ),
            ));
        }
        Err(throw_exception_ex(
            req,
            "ReflectionException",
            &format!("Property {shown}::${name} does not exist"),
        ))
    }

    /// `ReflectionClass::setStaticPropertyValue(string $name, mixed
    /// $value)`: the slot addressed under `BP_VAR_W` with the fake scope
    /// set to the class, which is [`read_class_prop_silent`]'s test with
    /// the typed-and-unwritten slot standing as a slot; a name the class
    /// does not have is php's `ReflectionException` in words of its own,
    /// `Class C does not have a property named p`, and never the engine's
    /// `Access to undeclared static property`. The write is
    /// `zend_update_static_property_ex` ([`write_static_prop`]), which
    /// writes through a slot holding a reference.
    ///
    /// php verifies the value against a typed property
    /// (`zend_verify_property_type`) and this engine does not yet, here or
    /// on `ReflectionProperty::setValue`; a value of the property's own
    /// type is written the same either way.
    ///
    /// ```php
    /// class A { private static $q = 'a'; public static $p = 'b'; }
    /// class B extends A { private static $q = 'c'; }
    /// $a = new ReflectionClass('A'); $b = new ReflectionClass('B');
    /// $a->setStaticPropertyValue('q', 'one'); $a->setStaticPropertyValue('p', 'two'); $b->setStaticPropertyValue('q', 'three');
    /// var_dump($a->getStaticProperties(), $b->getStaticProperties());
    /// ```
    /// ```expect
    /// array(2) {
    ///   ["q"]=>
    ///   string(3) "one"
    ///   ["p"]=>
    ///   string(3) "two"
    /// }
    /// array(2) {
    ///   ["q"]=>
    ///   string(5) "three"
    ///   ["p"]=>
    ///   string(3) "two"
    /// }
    /// ```
    ///
    /// A property with a declared type is held to it, the write being
    /// `zend_update_static_property_ex`'s: the weak rule's coercion however
    /// the file was declared, and php's `TypeError` for a value that
    /// neither satisfies the type nor coerces.
    ///
    /// ```php
    /// class T { public static int $n = 1; }
    /// $r = new ReflectionClass('T');
    /// $r->setStaticPropertyValue('n', "9"); var_dump($r->getStaticPropertyValue('n'));
    /// $r->setStaticPropertyValue('n', "x");
    /// ```
    /// ```expect exit=255
    /// int(9)
    ///
    /// Fatal error: Uncaught TypeError: Cannot assign string to property T::$n of type int in %s:5
    /// Stack trace:
    /// #0 %s(5): ReflectionClass->setStaticPropertyValue('n', 'x')
    /// #1 {main}
    ///   thrown in %s on line 5
    /// ```
    ///
    /// ```php
    /// class A { public $i = 1; }
    /// (new ReflectionClass('A'))->setStaticPropertyValue('i', 2);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Class A does not have a property named i in %s:3
    /// Stack trace:
    /// #0 %s(3): ReflectionClass->setStaticPropertyValue('i', 2)
    /// #1 {main}
    ///   thrown in %s on line 3
    /// ```
    ///
    /// ```php
    /// (new ReflectionClass('Exception'))->setStaticPropertyValue('p');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: ReflectionClass::setStaticPropertyValue() expects exactly 2 arguments, 1 given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClass->setStaticPropertyValue('p')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    #[php(tentative)]
    fn setStaticPropertyValue(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        let name = name.as_bytes().to_vec();
        let class = this_class(req, this.object())?;
        update_class_constants(req, class)?;
        let held = read_class_prop_silent(req, class, &name)?;
        let Some(held) = held else {
            let shown = class_name_of(req, class);
            return Err(throw_exception_ex(
                req,
                "ReflectionException",
                &format!(
                    "Class {} does not have a property named {}",
                    String::from_utf8_lossy(&shown),
                    String::from_utf8_lossy(&name)
                ),
            ));
        };
        held.give_up(req);
        let value = Owner::take(req, value);
        write_static_prop(req, class, &name, value)?;
        Ok(())
    }

    /// `ReflectionClass::getDefaultProperties()`: `add_class_vars` twice,
    /// the `static` properties then the instance ones, each in
    /// `properties_info` order, the class's own before the inherited, a
    /// parent's private ones skipped, and a typed property with no
    /// default skipped as its `UNDEF` slot is; each default is the
    /// expression it was written as, evaluated in the declaring class.
    ///
    /// ```php
    /// class Foo { protected static $s = 'foo'; protected $i = 'foo'; public $n; } var_dump((new ReflectionClass('Foo'))->getDefaultProperties(), (new ReflectionClass('ReflectionException'))->getDefaultProperties());
    /// ```
    /// ```expect
    /// array(3) {
    ///   ["s"]=>
    ///   string(3) "foo"
    ///   ["i"]=>
    ///   string(3) "foo"
    ///   ["n"]=>
    ///   NULL
    /// }
    /// array(4) {
    ///   ["message"]=>
    ///   string(0) ""
    ///   ["code"]=>
    ///   int(0)
    ///   ["file"]=>
    ///   string(0) ""
    ///   ["line"]=>
    ///   int(0)
    /// }
    /// ```
    #[php(tentative)]
    fn getDefaultProperties(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let class = this_class(req, this.object())?;
        let mut defaults = PhpArray::new(req)?;
        let table = linked_properties(req, class);
        for statics in [true, false] {
            for (property, at) in &table {
                if property.is_instance == statics
                    || (*at != class && property.visibility == Visibility::Private)
                {
                    continue;
                }
                let value = match &property.default {
                    Some(expr) => match eval_declared(req, expr, Some(*at)) {
                        Ok(value) => value,
                        Err(thrown) => {
                            defaults.give_up(req);
                            return Err(thrown);
                        }
                    },
                    None if property.ty.is_some() => continue,
                    None => Owner::null(),
                };
                defaults.set_str(req, &property.name.0, value)?;
            }
        }
        Ok(defaults)
    }

    /// `ReflectionClass::isIterable()` and its old spelling
    /// `isIterateable()`: whether the class implements `Traversable` and
    /// can be instantiated.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('Exception'))->isIterable());
    /// var_dump((new ReflectionClass('Exception'))->isIterateable());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isIterable(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this_class(req, this.object())?;
        if cannot_instantiate(req, class) {
            return Ok(false);
        }
        // `ce->get_iterator`, which a class that is no `Traversable`
        // may set itself, as `FFI\CData` does.
        if req.symbols().class_own_iterator(class) {
            return Ok(true);
        }
        let Some(traversable) = lookup_class(req, b"Traversable")? else {
            return Ok(false);
        };
        Ok(req.symbols().instance_of(class, traversable))
    }

    #[php(tentative, implementation = ReflectionClass::isIterable)]
    fn isIterateable(this: This<Self>) -> bool;

    /// `ReflectionClass::implementsInterface(string|ReflectionClass
    /// $interface)`: `instanceof_function` again, with php's `ReflectionException` for
    /// a name that is not an interface.
    ///
    /// ```php
    /// var_dump((new ReflectionClass('JsonException'))->implementsInterface('Throwable'));
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    ///
    /// ```php
    /// (new ReflectionClass('JsonException'))->implementsInterface('Exception');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Exception is not an interface in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClass->implementsInterface('Exception')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    #[php(tentative)]
    fn implementsInterface(
        req: &mut Request,
        this: This<Self>,
        interface: Union<Instance<ReflectionClass>, PhpString>,
    ) -> Result<bool, Thrown> {
        let interface = reflector_class(req, interface, "Interface")?;
        if kind_of(req, interface) != ClassKind::Interface {
            let name = class_name_of(req, interface);
            return Err(throw_exception_ex(
                req,
                "ReflectionException",
                &format!("{} is not an interface", String::from_utf8_lossy(&name)),
            ));
        }
        let class = this_class(req, this.object())?;
        Ok(req.symbols().instance_of(class, interface))
    }

    /// `ReflectionClass::getExtension()`: `ce->info.internal.module` as a
    /// `ReflectionExtension`, made as `reflection_extension_factory` makes
    /// it, `object_init_ex` and the name written, no constructor served;
    /// `null` for a user class. A class Zend declares itself is php's
    /// `Core` `zend_module_entry`, which this engine has no `Extension` value for yet
    /// (#379), so `Exception` answers `null` here and `Core` there; the
    /// fence stays off that class until the bundle names it.
    ///
    /// ```php
    /// class C {} var_dump((new ReflectionClass('ReflectionException'))->getExtension(), (new ReflectionClass('ReflectionException'))->getExtensionName(), (new ReflectionClass('C'))->getExtension(), (new ReflectionClass('C'))->getExtensionName());
    /// ```
    /// ```expect
    /// object(ReflectionExtension)#2 (1) {
    ///   ["name"]=>
    ///   string(10) "Reflection"
    /// }
    /// string(10) "Reflection"
    /// NULL
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getExtension(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::ReflectionExtension>>>, Thrown> {
        let class = this_class(req, this.object())?;
        let Some(name) = class_extension_name(req, class) else {
            return Ok(None);
        };
        let Some(reflector) = lookup_class(req, b"ReflectionExtension")? else {
            return Ok(None);
        };
        let object = new_object(req, reflector)?;
        set_name(req, *object, name.as_bytes())?;
        Ok(Some(object.into_instance()))
    }

    /// `ReflectionClass::getExtensionName()`: the extension's name, or `false`
    /// for a user class.
    ///
    /// ```php
    /// class C {}
    /// var_dump((new ReflectionClass('ArrayObject'))->getExtensionName(), (new ReflectionClass('C'))->getExtensionName());
    /// ```
    /// ```expect
    /// string(3) "SPL"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getExtensionName(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let class = this_class(req, this.object())?;
        match class_extension_name(req, class) {
            Some(name) => PhpString::new(req, name.as_bytes()).map(OrFalse::Value),
            None => Ok(OrFalse::False),
        }
    }

    /// `ReflectionClass::inNamespace()`: whether `ce->name` has a `\`.
    ///
    /// ```php
    /// $r = new ReflectionClass('ReflectionException'); var_dump($r->inNamespace(), $r->getNamespaceName(), $r->getShortName());
    /// ```
    /// ```expect
    /// bool(false)
    /// string(0) ""
    /// string(19) "ReflectionException"
    /// ```
    #[php(tentative)]
    fn inNamespace(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this_class(req, this.object())?;
        let name = class_name_of(req, class);
        Ok(namespace_end(&name).is_some())
    }

    /// `ReflectionClass::getNamespaceName()`: the name up to its last `\`,
    /// empty without one.
    ///
    /// ```php
    /// namespace N { class D {} }
    /// namespace { $d = new ReflectionClass('N\D'); $s = new ReflectionClass('stdClass');
    /// var_dump($d->getNamespaceName(), $s->getNamespaceName()); }
    /// ```
    /// ```expect
    /// string(1) "N"
    /// string(0) ""
    /// ```
    #[php(tentative)]
    fn getNamespaceName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let class = this_class(req, this.object())?;
        let name = class_name_of(req, class);
        let end = namespace_end(&name).unwrap_or(0);
        PhpString::new(req, &name[..end])
    }

    /// `ReflectionClass::getShortName()`: the name after its last `\`, the
    /// whole name without one.
    ///
    /// ```php
    /// namespace N { class D {} }
    /// namespace { var_dump((new ReflectionClass('N\D'))->getShortName(), (new ReflectionClass('stdClass'))->getShortName()); }
    /// ```
    /// ```expect
    /// string(1) "D"
    /// string(8) "stdClass"
    /// ```
    #[php(tentative)]
    fn getShortName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let class = this_class(req, this.object())?;
        let name = class_name_of(req, class);
        let start = namespace_end(&name).map_or(0, |end| end + 1);
        PhpString::new(req, &name[start..])
    }

    /// `ReflectionClass::getAttributes(?string $name = null, int $flags =
    /// 0)`: `reflect_attributes` over `ce->attributes` with
    /// `ZEND_ATTRIBUTE_TARGET_CLASS`, an empty array for a class with none.
    ///
    /// ```php
    /// class C {} var_dump((new ReflectionClass('C'))->getAttributes(), (new ReflectionClass('ReflectionClass'))->getAttributes());
    /// try { (new ReflectionClass('C'))->getAttributes(null, 4); } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// try { (new ReflectionClass('C'))->getAttributes('Nope', ReflectionAttribute::IS_INSTANCEOF); } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// array(0) {
    /// }
    /// array(0) {
    /// }
    /// ValueError: ReflectionClass::getAttributes(): Argument #2 ($flags) must be a valid attribute filter flag
    /// Error: Class "Nope" not found
    /// ```
    ///
    /// ```php
    /// (new ReflectionClass('ReflectionException'))->getAttributes(1, 2, 3);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: ReflectionClass::getAttributes() expects at most 2 arguments, 3 given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClass->getAttributes(1, 2, 3)
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    fn getAttributes(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] name: Option<PhpString>,
        #[php(default = 0)] flags: i64,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let class = this_class(req, this.object())?;
        let (attributes, file) = req.symbols().class_decl(class).map_or_else(
            || (Vec::new(), None),
            |decl| (decl.attributes.clone(), attr::declared_in(&decl.file.0)),
        );
        attr::reflect_attributes(
            req,
            &attributes,
            attr::Subject::new(Some(class), attr::TARGET_CLASS, file.as_deref()),
            name,
            flags,
        )
    }
}

/// `ReflectionObject` as the stub declares it.
#[php_class(extends(ReflectionClass), size = 96, payload = crate::Reflected, handlers = crate::HANDLERS)]
pub(crate) struct ReflectionObject;

#[php_class]
impl ReflectionObject {
    /// `reflection_class_object_ctor` with `is_object` on,
    /// `ReflectionObject::__construct(object $object)`: the object's class
    /// as `$name`, and the object itself as `intern->obj`, one more owner,
    /// which the `Dynamic properties` block of `__toString` reads and
    /// `free_obj` gives back.
    ///
    /// ```php
    /// $t = new stdClass; $t->b = 2;
    /// echo new ReflectionObject($t);
    /// ```
    /// ```expect
    /// Object of class [ <internal:Core> class stdClass ] {
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
    ///   - Properties [0] {
    ///   }
    ///
    ///   - Dynamic properties [1] {
    ///     Property [ <dynamic> public $b ]
    ///   }
    ///
    ///   - Methods [0] {
    ///   }
    /// }
    /// ```
    ///
    /// ```php
    /// var_dump(new ReflectionObject(new ReflectionException));
    /// ```
    /// ```expect
    /// object(ReflectionObject)#1 (1) {
    ///   ["name"]=>
    ///   string(19) "ReflectionException"
    /// }
    /// ```
    ///
    /// ```php
    /// new ReflectionObject('x');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught TypeError: ReflectionObject::__construct(): Argument #1 ($object) must be of type object, string given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionObject->__construct('x')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    fn __construct(req: &mut Request, this: This<Self>, object: PhpObject) -> Result<(), Thrown> {
        let name = class_name_of(req, object.class());
        let this = this.object();
        set_name(req, this, &name)?;
        keep(req, this, object.class(), Some(object));
        Ok(())
    }
}
