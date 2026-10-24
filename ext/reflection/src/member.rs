//! The accessors `php_reflection.c` answers off one property, one
//! class constant and one global constant, and the one method
//! `Reflection` itself declares. Each reads the declaration the reflector's `$name`,
//! and a member's `$class`, look up again.
//!
//! A property's hooks are its declaration's with what it inherits, as
//! `ce->properties_info` holds them once linked, which the engine
//! resolves ([`php_ext::call::property_hook`]); a hook is reflected as a
//! `ReflectionMethod` named `$name::get` or `$name::set` (#427).

use php_ext::abi::{Instance, OrFalse, Passed, PropertyRead, PropertyTest, This, Union};
use php_ext::attr::php_class;
use php_ext::call::{
    constant, eval_declared, property_hook, property_is_virtual, read_class_prop_silent,
    read_static_prop, skip_lazy_initialization, write_static_prop,
};
use php_ext::decl::{
    ClassConstDecl, ClassKind, HookKind, MethodDecl, PropertyDecl, Type, Visibility,
};
use php_ext::raising::{Thrown, arg_count_error, arg_type_error, deprecated, throw, throw_error};
use php_ext::request::Request;
use php_ext::values::{ClassId, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use super::lazy::{lazy_compatible, object_of};
use super::signature::type_factory;
use super::{
    Reference, asked_class, attr, class_const_string, class_constant_named, class_name_of,
    class_named, class_of, const_string, constant_key, dynamic_properties, effective_prop, held_to,
    keep, keep_reference, kept_class, namespace_end, property_named, property_string,
    reflection_class_factory, set_name, set_string_prop, string_value, this_class_constant,
    this_name, this_property,
};

// ----------------------------------------------- ReflectionProperty

/// php's `property_reference`: the class the reflector names, the
/// property's name, and the declaration with the class that declared
/// it, `None` for php's dynamic property.
type Reflected = (ClassId, Vec<u8>, Option<(ClassId, PropertyDecl)>);

/// `GET_REFLECTION_OBJECT_PTR(ref)` for a property reflector, with the
/// class that declared it.
fn this_declared(req: &mut Request, this: PhpObject) -> Result<Reflected, Thrown> {
    let name = this_name(req, this, "ReflectionProperty")?;
    let Some(class) = this
        .prop(req, b"class")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Err(throw_error(
            req,
            "Error",
            "Typed property ReflectionProperty::$class must not be accessed before initialization",
        ));
    };
    let class = class_named(req, &class)?;
    let found = property_named(req, class, &name);
    Ok((class, name, found))
}

/// A hook's name read back into the property it hooks and its kind,
/// `$name::get` or `$name::set` as `zend_compile_property_hooks` names
/// it.
pub(super) fn hook_name(name: &[u8]) -> Option<(&[u8], HookKind)> {
    let rest = name.strip_prefix(b"$")?;
    if let Some(property) = rest.strip_suffix(b"::get") {
        return Some((property, HookKind::Get));
    }
    rest.strip_suffix(b"::set")
        .map(|property| (property, HookKind::Set))
}

/// `ref->prop->hooks[kind]`: the hook of `kind` a declared property's
/// table entry carries, with the class that wrote it; none for a
/// dynamic property.
fn hook_of(
    req: &Request,
    found: Option<&(ClassId, PropertyDecl)>,
    name: &[u8],
    kind: HookKind,
) -> Option<(ClassId, MethodDecl)> {
    let (declaring, _) = found?;
    property_hook(req, *declaring, name, kind)
}

/// `reflection_method_factory(hook->common.scope, hook, NULL, ...)`: a
/// `ReflectionMethod` over one hook, its `$class` the hook's scope.
fn hook_reflector(
    req: &mut Request,
    scope: ClassId,
    decl: &MethodDecl,
) -> Result<Owner<PhpObject>, Thrown> {
    let object = super::class::method_factory(req, &decl.function.name.0, scope, scope, None)?;
    super::keep_hook(req, *object);
    Ok(object)
}

/// The visibility, set visibility, `final`, `static` and `readonly`
/// bits of a property's `flags`, the numbers the stub's `IS_*`
/// constants name.
fn property_bits(decl: &PropertyDecl) -> i64 {
    let mut bits = match decl.visibility {
        Visibility::Public => 1,
        Visibility::Protected => 2,
        Visibility::Private => 4,
    };
    match decl.set_visibility {
        Some(Visibility::Private) => bits |= 4096,
        Some(_) => bits |= 2048,
        None => {}
    }
    if decl.is_final {
        bits |= 32;
    }
    if !decl.is_instance {
        bits |= 16;
    }
    if decl.is_readonly {
        bits |= 128;
    }
    bits
}

/// The two `(set)` accessors' body: a dynamic property carries no set
/// visibility, so it is `false` for either.
fn set_visibility_is(
    req: &mut Request,
    this: PhpObject,
    wanted: Visibility,
) -> Result<bool, Thrown> {
    let (_, _, found) = this_declared(req, this)?;
    Ok(found.is_some_and(|(_, decl)| decl.set_visibility == Some(wanted)))
}

/// `prop_info->flags` as far as the stub's `IS_*` constants name them:
/// [`property_bits`], then `ZEND_ACC_ABSTRACT` and `ZEND_ACC_VIRTUAL`,
/// which the table of `declaring` settles.
pub(super) fn property_flags(
    req: &Request,
    declaring: ClassId,
    name: &[u8],
    decl: &PropertyDecl,
) -> i64 {
    let mut bits = property_bits(decl);
    if decl.hooks.as_ref().is_some_and(|hooks| hooks.is_abstract) {
        bits |= 64;
    }
    if property_is_virtual(req, declaring, name) {
        bits |= 512;
    }
    bits
}

fn visibility_is(
    req: &mut Request,
    this: PhpObject,
    wanted: Visibility,
    dynamic: bool,
) -> Result<bool, Thrown> {
    let (_, _, found) = this_declared(req, this)?;
    Ok(found.map_or(dynamic, |(_, decl)| decl.visibility == wanted))
}

/// `property_get_default`'s three answers: a written default, the
/// implicit `null` of an untyped property, and nothing, which a typed
/// property without one, a virtual one and a dynamic property give.
enum Default {
    Written(php_ext::decl::ConstExpr, ClassId),
    ImplicitNull,
    Absent,
}

/// [`Default`] of the property a reflector names.
fn default_of(req: &Request, name: &[u8], found: Option<(ClassId, PropertyDecl)>) -> Default {
    match found {
        Some((scope, _)) if property_is_virtual(req, scope, name) => Default::Absent,
        Some((scope, decl)) => match (decl.default, decl.ty) {
            (Some(expr), _) => Default::Written(expr, scope),
            (None, None) => Default::ImplicitNull,
            (None, Some(_)) => Default::Absent,
        },
        None => Default::Absent,
    }
}

// ------------------------------------------ ReflectionClassConstant

/// `GET_REFLECTION_OBJECT_PTR(ref)` for a class-constant reflector,
/// with the class that declared it.
fn this_constant(
    req: &mut Request,
    this: PhpObject,
) -> Result<(Vec<u8>, ClassId, ClassConstDecl), Thrown> {
    let name = this_name(req, this, "ReflectionClassConstant")?;
    let Some(class) = this
        .prop(req, b"class")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Err(throw_error(
            req,
            "Error",
            "Typed property ReflectionClassConstant::$class must not be accessed before initialization",
        ));
    };
    let class = class_named(req, &class)?;
    let Some((scope, decl)) = class_constant_named(req, class, &name) else {
        let named = class_name_of(req, class);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Constant {}::{} does not exist",
                String::from_utf8_lossy(&named),
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    Ok((name, scope, decl))
}

fn constant_visibility_is(
    req: &mut Request,
    this: PhpObject,
    wanted: Visibility,
) -> Result<bool, Thrown> {
    let (_, _, decl) = this_constant(req, this)?;
    Ok(decl.visibility == wanted)
}

// ----------------------------------------------- ReflectionConstant

/// The name a `ReflectionConstant` reflects, with the namespace part
/// lowered as its constructor lowers it for the lookup.
fn this_global_constant(req: &mut Request, this: PhpObject) -> Result<Vec<u8>, Thrown> {
    let name = this_name(req, this, "ReflectionConstant")?;
    Ok(name.strip_prefix(b"\\").unwrap_or(&name).to_vec())
}

/// `reflection_constant_find_ext`: the extension whose constants
/// table the name is in, php's walk of `EG(zend_constants)` by the
/// number that says which extension declared it.
fn constant_extension_name(req: &mut Request, this: PhpObject) -> Result<Option<String>, Thrown> {
    let name = this_global_constant(req, this)?;
    let extension = req.symbols().constant_extension(&name);
    Ok(extension
        .and_then(|at| req.extensions().name(at))
        .map(str::to_owned))
}

/// `reflection_property_set_raw_value`: `value`, a view of the
/// caller's, written past the effective property's `set` hook where it
/// has one, by calling the hook's trampoline on the object as php does
/// ([`php_ext::call::write_raw_property`]), and otherwise through
/// `write_property` under `EG(fake_scope)`,
/// the class the reflector was asked about, so a parent's private
/// property the object's class redeclares is the parent's slot.
///
/// ```php
/// class B { private $p = 'B'; }
/// class T extends B { private $p = 'T' { get => $this->p; set => $value; } }
/// $r = new ReflectionProperty('B', 'p');
/// $o = new T;
/// $r->setRawValue($o, 42);
/// var_dump($r->getRawValue($o), (new ReflectionProperty('T', 'p'))->getRawValue($o));
/// ```
/// ```expect
/// int(42)
/// string(1) "T"
/// ```
fn set_raw_value(
    req: &mut Request,
    asked: ClassId,
    prop: Option<&(ClassId, PropertyDecl)>,
    object: PhpObject,
    name: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    if let Some((at, _)) =
        prop.filter(|(at, _)| property_hook(req, *at, name, HookKind::Set).is_some())
    {
        return php_ext::call::write_raw_property(req, &object, *at, name, value);
    }
    let value = Owner::take(req, value);
    let key = match PhpString::new(req, name) {
        Ok(key) => key,
        Err(thrown) => {
            value.give_up(req);
            return Err(thrown);
        }
    };
    let handlers = object.handlers(req);
    req.frames_mut().with_fake_scope(asked, |req| {
        handlers.write_property(req, object, key, value)
    })
}

/// `Reflection` as the stub declares it.
#[php_class]
pub(crate) struct Reflection;

#[php_class]
impl Reflection {
    /// `Reflection::getModifierNames(int $modifiers)`: the words php
    /// prints for the bits, in `add_next_index`'s order, the three
    /// visibilities mutually exclusive and so are the two `(set)` ones.
    ///
    /// ```php
    /// var_dump(Reflection::getModifierNames(33));
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   string(5) "final"
    ///   [1]=>
    ///   string(6) "public"
    /// }
    /// ```
    #[php(tentative)]
    fn getModifierNames(req: &mut Request, modifiers: i64) -> Result<Owner<PhpArray>, Thrown> {
        let mut words: Vec<&str> = Vec::new();
        if modifiers & 64 != 0 {
            words.push("abstract");
        }
        if modifiers & 32 != 0 {
            words.push("final");
        }
        if modifiers & 512 != 0 {
            words.push("virtual");
        }
        match modifiers & 7 {
            1 => words.push("public"),
            4 => words.push("private"),
            2 => words.push("protected"),
            _ => {}
        }
        match modifiers & (2048 | 4096) {
            2048 => words.push("protected(set)"),
            4096 => words.push("private(set)"),
            _ => {}
        }
        if modifiers & 16 != 0 {
            words.push("static");
        }
        if modifiers & (128 | 65536) != 0 {
            words.push("readonly");
        }
        let mut names = PhpArray::new(req)?;
        for word in words {
            let value = string_value(req, word.as_bytes())?;
            names.append(req, value)?;
        }
        Ok(names)
    }
}

/// `ReflectionProperty` as the stub declares it. Its constructor and
/// `reflection_property_factory` lay a `property_reference` down beside
/// it, given back with it:
///
/// ```php
/// $w = new ReflectionProperty('Exception', 'message');
/// $m = memory_get_usage(); $r = new ReflectionProperty('Exception', 'message'); $n = memory_get_usage(); unset($r);
/// var_dump($n - $m, memory_get_usage() - $n);
/// ```
/// ```expect
/// int(152)
/// int(-152)
/// ```
#[php_class(implements(crate::Reflector), not_serializable, size = 96, payload = crate::Reflected, handlers = crate::HANDLERS)]
pub(crate) struct ReflectionProperty;

#[php_class]
impl ReflectionProperty {
    const IS_STATIC: i64 = 16;
    const IS_READONLY: i64 = 128;
    const IS_PUBLIC: i64 = 1;
    const IS_PROTECTED: i64 = 2;
    const IS_PRIVATE: i64 = 4;
    const IS_ABSTRACT: i64 = 64;
    const IS_PROTECTED_SET: i64 = 2048;
    const IS_PRIVATE_SET: i64 = 4096;
    const IS_VIRTUAL: i64 = 512;
    const IS_FINAL: i64 = 32;
    #[php(property)]
    const name: PhpString;
    #[php(property)]
    const class: PhpString;

    #[php(private, implementation = crate::func::ReflectionFunctionAbstract::__clone)]
    fn __clone(this: This<Self>);

    /// `ReflectionProperty::__construct(object|string $class, string
    /// $property)`: the property in the class's info table, or a dynamic
    /// one on the object given, else php's `ReflectionException`; `$name` is the name
    /// and `$class` the declaring class, `property_info->ce->name`.
    ///
    /// ```php
    /// var_dump(new ReflectionProperty('ReflectionException', 'message'));
    /// ```
    /// ```expect
    /// object(ReflectionProperty)#1 (2) {
    ///   ["name"]=>
    ///   string(7) "message"
    ///   ["class"]=>
    ///   string(9) "Exception"
    /// }
    /// ```
    ///
    /// ```php
    /// new ReflectionProperty('ReflectionException', 'string');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Property ReflectionException::$string does not exist in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionProperty->__construct('ReflectionExcep...', 'string')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// new ReflectionProperty('Nope', 'x');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Class "Nope" does not exist in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionProperty->__construct('Nope', 'x')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// new ReflectionProperty('ReflectionException');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: ReflectionProperty::__construct() expects exactly 2 arguments, 1 given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionProperty->__construct('ReflectionExcep...')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// A parent's private property is not the child's, from a class or from
    /// an object:
    ///
    /// ```php
    /// class P { private $p = 1; } class C extends P {}
    /// foreach (['C', new C] as $from) { try { new ReflectionProperty($from, 'p'); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; } }
    /// ```
    /// ```expect
    /// Property C::$p does not exist
    /// Property C::$p does not exist
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        class: Union<PhpObject, PhpString>,
        property: PhpString,
    ) -> Result<(), Thrown> {
        let (class, object) = class_of(req, class)?;
        let name = property.as_bytes().to_vec();
        let declaring = match property_named(req, class, &name) {
            Some((declaring, _)) => declaring,
            // The dynamic arm: a name in the object's table the class does
            // not declare, and never a parent's private property, which the
            // table holds under its mangled key.
            None if object.is_some_and(|o| dynamic_properties(req, class, o).contains(&name)) => {
                class
            }
            None => {
                let class = class_name_of(req, class);
                return Err(throw(
                    req,
                    "ReflectionException",
                    &format!(
                        "Property {}::${} does not exist",
                        String::from_utf8_lossy(&class),
                        String::from_utf8_lossy(&name)
                    ),
                ));
            }
        };
        let declaring = class_name_of(req, declaring);
        let this = this.object();
        set_name(req, this, &name)?;
        set_string_prop(req, this, b"class", &declaring)?;
        // `intern->ce = ce`: the class asked about, which `getValue`
        // holds its argument to, where `$class` is the declaring one;
        // `intern->ptr` its `property_reference`.
        keep(req, this, class, None);
        keep_reference(req, this, Reference::Property)
    }

    /// `ReflectionProperty::__toString()`: `_property_string` over the
    /// property info, the row `_class_string` prints inside its
    /// `Properties` block.
    ///
    /// ```php
    /// echo new ReflectionProperty('ReflectionClass', 'name');
    /// echo new ReflectionProperty('Exception', 'message');
    /// ```
    /// ```expect
    /// Property [ public string $name ]
    /// Property [ protected $message = '' ]
    /// ```
    ///
    /// ```php
    /// $o = new stdClass; $o->x = 1;
    /// echo new ReflectionProperty($o, 'x');
    /// ```
    /// ```expect
    /// Property [ <dynamic> public $x ]
    /// ```
    ///
    /// ```php
    /// abstract class A { abstract public int $a { get; } public $b = 1 { final set => $value; } }
    /// echo new ReflectionProperty('A', 'a'), new ReflectionProperty('A', 'b');
    /// ```
    /// ```expect
    /// Property [ abstract public virtual int $a { get; } ]
    /// Property [ public $b = 1 { final set; } ]
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let (_, name, found) = this_declared(req, this.object())?;
        let Some((declaring, decl)) = found else {
            // `_property_string` with no property info: php's dynamic row,
            // which carries no modifier but `public` and no default.
            let text = format!(
                "Property [ <dynamic> public ${} ]\n",
                String::from_utf8_lossy(&name)
            );
            return PhpString::new(req, text.as_bytes());
        };
        let text = property_string(req, declaring, &decl, "");
        PhpString::new(req, text.as_bytes())
    }

    /// `ReflectionProperty::getName()`: `ref->unmangled_name`.
    ///
    /// ```php
    /// var_dump((new ReflectionProperty('Exception', 'message'))->getName());
    /// ```
    /// ```expect
    /// string(7) "message"
    /// ```
    #[php(tentative)]
    fn getName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let (_, name, _) = this_declared(req, this.object())?;
        PhpString::declared(req, &name)
    }

    /// `ReflectionProperty::getMangledName()`: `prop->name`, the key the
    /// property has in the object's table, as `zend_mangle_property_name`
    /// writes it: the name for a public property, `\0*\0name` for a
    /// protected one and `\0Class\0name`, the declaring class, for a
    /// private one; a dynamic property has no info and answers
    /// `ref->unmangled_name`.
    ///
    /// ```php
    /// class P { public $a; protected $b; private $c; }
    /// $o = new P; $o->d = 1;
    /// foreach (['a', 'b', 'c'] as $n) { var_dump((new ReflectionProperty('P', $n))->getMangledName()); }
    /// var_dump((new ReflectionProperty($o, 'd'))->getMangledName(), array_keys((array) $o));
    /// ```
    /// ```expect
    /// Deprecated: Creation of dynamic property P::$d is deprecated in %s on line 3
    /// string(1) "a"
    /// string(4) "%0*%0b"
    /// string(4) "%0P%0c"
    /// string(1) "d"
    /// array(4) {
    ///   [0]=>
    ///   string(1) "a"
    ///   [1]=>
    ///   string(4) "%0*%0b"
    ///   [2]=>
    ///   string(4) "%0P%0c"
    ///   [3]=>
    ///   string(1) "d"
    /// }
    /// ```
    fn getMangledName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let (_, name, found) = this_declared(req, this.object())?;
        let mangled = match found {
            Some((scope, decl)) => match decl.visibility {
                Visibility::Public => name,
                Visibility::Protected => [b"\0*\0".as_slice(), &name].concat(),
                Visibility::Private => {
                    let class = class_name_of(req, scope);
                    [b"\0".as_slice(), &class, b"\0", &name].concat()
                }
            },
            None => name,
        };
        PhpString::new(req, &mangled)
    }

    /// `ReflectionProperty::getValue(?object $object = null)`: an instance
    /// property read off the object given, which must be of the declaring
    /// class, through the object's own `read_property` under
    /// `EG(fake_scope) = intern->ce`, the class the reflector was asked
    /// about, so visibility is measured from there: the slot or the
    /// dynamic table by name, a `get` hook served, a `__get` asked where
    /// nothing holds the name, and whatever a class's handler serves in
    /// place of a slot, a DOM node's `@virtual` properties among them. A
    /// slot holds the declared default, which `object_properties_init` put
    /// there when the object was made, an inherited property's included
    /// (#419); a typed property with no default holds nothing and is php's
    /// `Error` for a read before initialization. A `static` property is
    /// `zend_read_static_property_ex` over the class the reflector was
    /// asked about (`intern->ce`), whose fake scope is that class, and the
    /// argument is neither read nor required; php parses the argument
    /// before it looks at the property, so what is no object is a
    /// `TypeError` either way.
    ///
    /// ```php
    /// class P { public static $s = 1; } class C extends P {}
    /// $r = new ReflectionProperty('C', 's');
    /// var_dump($r->class, $r->getValue(), $r->getValue(null));
    /// ```
    /// ```expect
    /// string(1) "P"
    /// int(1)
    /// int(1)
    /// ```
    ///
    /// ```php
    /// class T { protected $p = 2; } $o = new T; var_dump((new ReflectionProperty($o, 'p'))->getValue($o), (new ReflectionProperty('Exception', 'message'))->getValue(new ReflectionException), (new ReflectionProperty('Exception', 'code'))->getValue(new Exception));
    /// ```
    /// ```expect
    /// int(2)
    /// string(0) ""
    /// int(0)
    /// ```
    ///
    /// ```php
    /// (new ReflectionProperty('Exception', 'message'))->getValue();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught TypeError: ReflectionProperty::getValue(): Argument #1 ($object) must be provided for instance properties in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionProperty->getValue()
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// class T { protected $p = 2; } (new ReflectionProperty('Exception', 'message'))->getValue(new T);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Given object is not an instance of the class this property was declared in in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionProperty->getValue(Object(T))
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// (new ReflectionProperty('Exception', 'message'))->getValue(1);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught TypeError: ReflectionProperty::getValue(): Argument #1 ($object) must be of type ?object, int given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionProperty->getValue(1)
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// An object of the declaring class is read through a reflector asked
    /// about a subclass, and a missing dynamic property is php's warning:
    ///
    /// ```php
    /// class A { public $p = 'p'; } class B extends A {}
    /// $b = new A; $b->d = 1;
    /// var_dump((new ReflectionProperty('B', 'p'))->getValue(new A), (new ReflectionProperty($b, 'd'))->getValue(new A));
    /// ```
    /// ```expect
    /// Deprecated: Creation of dynamic property A::$d is deprecated in %s on line 3
    ///
    /// Warning: Undefined property: A::$d in %s on line 4
    /// string(1) "p"
    /// NULL
    /// ```
    ///
    /// A private property is the slot the reflector's class sees, the
    /// parent's where the object's class redeclares it, and one an
    /// `unset` emptied asks `__get`:
    ///
    /// ```php
    /// class A { private $p = "A"; } class B extends A { private $p = "B"; }
    /// var_dump((new ReflectionProperty('A', 'p'))->getValue(new B), (new ReflectionClass('B'))->getProperty('A::p')->getValue(new B));
    /// class G { private $g; function __construct() { unset($this->g); } function __get($n) { return "get $n"; } }
    /// var_dump((new ReflectionProperty('G', 'g'))->getValue(new G));
    /// ```
    /// ```expect
    /// string(1) "A"
    /// string(1) "A"
    /// string(5) "get g"
    /// ```
    ///
    /// A property the class's handler serves has no slot, and is what
    /// the handler answers:
    ///
    /// ```php
    /// $d = new DOMDocument; $d->loadXML('<r>t</r>');
    /// var_dump((new ReflectionProperty('DOMNode', 'nodeType'))->getValue($d->documentElement));
    /// var_dump((new ReflectionProperty('DOMNode', 'textContent'))->getValue($d->documentElement));
    /// var_dump((new ReflectionProperty('DOMDocument', 'documentElement'))->getValue($d)->nodeName);
    /// var_dump((new ReflectionProperty('XMLReader', 'nodeType'))->getValue(new XMLReader));
    /// ```
    /// ```expect
    /// int(1)
    /// string(1) "t"
    /// string(1) "r"
    /// int(0)
    /// ```
    #[php(tentative)]
    fn getValue(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] object: Option<PhpObject>,
    ) -> Result<Owner<Value>, Thrown> {
        let (class, name, declared) = this_property(req, this.object())?;
        // A `static` property is read through `intern->ce`, the class the
        // reflector was asked about.
        let asked = asked_class(req, this.object(), class);
        if declared.as_ref().is_some_and(|decl| !decl.is_instance) {
            return read_static_prop(req, asked, &name);
        }
        let Some(object) = object else {
            return Err(arg_type_error(
                req,
                1,
                "be provided for instance properties",
            ));
        };
        // `ref->prop ? ref->prop->ce : intern->ce`: a declared property's
        // object is held to the class that declared it, a dynamic one's to
        // the class the reflector was asked about.
        let holder = if declared.is_some() { class } else { asked };
        held_to(req, object, holder)?;
        let handlers = object.handlers(req);
        req.frames_mut().with_fake_scope(asked, |req| {
            handlers.read_property(req, object, &name, PropertyRead::Warning)
        })
    }

    /// `ReflectionProperty::setValue(mixed $objectOrValue, mixed $value)`:
    /// an instance property written on the object given through
    /// `write_property` under `fake_scope`, one more owner of the value
    /// taken for the slot. A `static` property takes php's two 8.3
    /// deprecations, the one for a single argument and the one for a first
    /// argument that is neither `null` nor an object, and is then
    /// `zend_update_static_property_ex` over the class the reflector was
    /// asked about; the value is the last argument either way.
    ///
    /// ```php
    /// class T { public static $s = 1; } $r = new ReflectionProperty('T', 's'); var_dump($r->getValue()); $r->setValue(null, 2); var_dump($r->getValue());
    /// ```
    /// ```expect
    /// int(1)
    /// int(2)
    /// ```
    ///
    /// A deprecation a handler throws from ends the call
    /// before the write:
    ///
    /// ```php
    /// class T { public static $s = 1; }
    /// set_error_handler(function () { throw new Exception('d'); });
    /// try { (new ReflectionProperty('T', 's'))->setValue('T', 2); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// var_dump(T::$s);
    /// ```
    /// ```expect
    /// d
    /// int(1)
    /// ```
    ///
    /// The single-argument form, php's 8.3 deprecation, whose value is the
    /// one argument:
    ///
    /// ```php
    /// class T { public static $s = 1; }
    /// $r = new ReflectionProperty('T', 's');
    /// $r->setValue(3);
    /// var_dump(T::$s);
    /// ```
    /// ```expect
    /// Deprecated: Calling ReflectionProperty::setValue() with a single argument is deprecated in %s on line 4
    /// int(3)
    /// ```
    ///
    /// ```php
    /// class T { public $p = 1; } $o = new T; $r = new ReflectionProperty($o, 'p'); $r->setValue($o, 'two'); var_dump($r->getValue($o), $o->p);
    /// ```
    /// ```expect
    /// string(3) "two"
    /// string(3) "two"
    /// ```
    ///
    /// ```php
    /// class T { public $p = 1; } (new ReflectionProperty('T', 'p'))->setValue(new T);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: ReflectionProperty::setValue() expects exactly 2 arguments, 1 given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionProperty->setValue(Object(T))
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// A write into a `static` property with a declared type is held to it,
    /// `zend_update_static_property_ex` verifying the value and coercing it
    /// by the weak rule (`ops::verify_property_type`), so what Reflection
    /// puts in the slot is what the type accepts and never a value of
    /// another type.
    ///
    /// ```php
    /// class T { public static int $n = 1; }
    /// $r = new ReflectionProperty('T', 'n');
    /// $r->setValue(null, "7"); var_dump($r->getValue());
    /// $r->setValue(null, [1]);
    /// ```
    /// ```expect exit=255
    /// int(7)
    ///
    /// Fatal error: Uncaught TypeError: Cannot assign array to property T::$n of type int in %s:5
    /// Stack trace:
    /// #0 %s(5): ReflectionProperty->setValue(NULL, Array)
    /// #1 {main}
    ///   thrown in %s on line 5
    /// ```
    ///
    /// ```php
    /// (new ReflectionProperty('Exception', 'message'))->setValue(1, 2);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught TypeError: ReflectionProperty::setValue(): Argument #1 ($objectOrValue) must be of type object, int given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionProperty->setValue(1, 2)
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// The write is the object's own `write_property` under the fake
    /// scope of the class the reflector was asked about, so a parent's
    /// private property is the parent's slot where the object's class
    /// redeclares it, and a DOM node's property is what its handler
    /// writes:
    ///
    /// ```php
    /// class A { private $p = "A"; } class B extends A { private $p = "B"; }
    /// $b = new B; (new ReflectionProperty('A', 'p'))->setValue($b, 'a');
    /// var_dump((new ReflectionProperty('A', 'p'))->getValue($b), (new ReflectionProperty('B', 'p'))->getValue($b));
    /// $d = new DOMDocument; $d->loadXML('<r>t</r>');
    /// (new ReflectionProperty('DOMNode', 'textContent'))->setValue($d->documentElement, 'x');
    /// echo $d->saveXML($d->documentElement), "\n";
    /// ```
    /// ```expect
    /// string(1) "a"
    /// string(1) "B"
    /// <r>x</r>
    /// ```
    #[php(tentative)]
    fn setValue(
        req: &mut Request,
        this: This<Self>,
        objectOrValue: Value,
        value: Passed<Value>,
    ) -> Result<(), Thrown> {
        let (class, name, declared) = this_property(req, this.object())?;
        if declared.as_ref().is_some_and(|decl| !decl.is_instance) {
            let value = match value {
                Passed::Omitted => {
                    deprecated(
                        req,
                        "Calling ReflectionProperty::setValue() with a single argument is deprecated",
                    );
                    php_ext::raising::pending(req)?;
                    objectOrValue
                }
                Passed::Given(value) => {
                    if !matches!(objectOrValue.as_ref(), ValueRef::Null | ValueRef::Object(_)) {
                        deprecated(
                            req,
                            "Calling ReflectionProperty::setValue() with a 1st argument which is not null or an object is deprecated",
                        );
                        php_ext::raising::pending(req)?;
                    }
                    value
                }
            };
            let asked = req
                .frames()
                .this()
                .and_then(|this| kept_class(req, this))
                .unwrap_or(class);
            let value = Owner::take(req, value);
            write_static_prop(req, asked, &name, value)?;
            return Ok(());
        }
        // `ZEND_PARSE_PARAMETERS_START(2, 2)` of the instance arm.
        let Passed::Given(value) = value else {
            return Err(arg_count_error(req, "exactly 2 arguments, 1 given"));
        };
        // Its `Z_PARAM_OBJ`, where the stub declares `mixed
        // $objectOrValue`.
        let given = php_ext::coerce::coerce_param(req, &objectOrValue, &Type::Object, 1)?;
        let Some(object) = given.as_object() else {
            return Ok(());
        };
        let key = PhpString::new(req, &name)?;
        let value = Owner::take(req, value);
        let (object, handlers) = (object, object.handlers(req));
        let asked = asked_class(req, this.object(), class);
        req.frames_mut().with_fake_scope(asked, |req| {
            handlers.write_property(req, object, key, value)
        })
    }

    /// `ReflectionProperty::getRawValue(object $object)`: the value as
    /// stored, past the `get` hook of the property the object's class holds
    /// ([`php_ext::call::read_raw_property`], which a virtual property
    /// refuses); with no such hook, `getValue`'s instance arm, the object
    /// held to `intern->ce`; a `static` property is php's
    /// `ReflectionException`.
    ///
    /// ```php
    /// class C { public $p = 1; private int $q = 2; public static $s; } $o = new C;
    /// var_dump((new ReflectionProperty('C', 'p'))->getRawValue($o), (new ReflectionProperty('C', 'q'))->getRawValue($o));
    /// try { (new ReflectionProperty('C', 's'))->getRawValue($o); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// try { (new ReflectionProperty('C', 'p'))->getRawValue(new stdClass); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// class H { public $b = 3 { get => $this->b * 100; } public $v { get => 1; } }
    /// var_dump((new ReflectionProperty('H', 'b'))->getRawValue(new H));
    /// try { (new ReflectionProperty('H', 'v'))->getRawValue(new H); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(1)
    /// int(2)
    /// May not use getRawValue on static properties
    /// Given object is not an instance of the class this property was declared in
    /// int(3)
    /// Must not read from virtual property H::$v
    /// ```
    fn getRawValue(
        req: &mut Request,
        this: This<Self>,
        object: PhpObject,
    ) -> Result<Owner<Value>, Thrown> {
        let (class, name, declared) = this_property(req, this.object())?;
        let asked = asked_class(req, this.object(), class);
        held_to(req, object, asked)?;
        let reflected = declared.map(|decl| (class, decl));
        let prop = effective_prop(req, asked, object, reflected, &name);
        if prop.as_ref().is_some_and(|(_, decl)| !decl.is_instance) {
            return Err(throw(
                req,
                "ReflectionException",
                "May not use getRawValue on static properties",
            ));
        }
        if let Some((at, _)) =
            prop.filter(|(at, _)| property_hook(req, *at, &name, HookKind::Get).is_some())
        {
            return php_ext::call::read_raw_property(req, &object, at, &name);
        }
        // `read_property` under `EG(fake_scope)`, the class the reflector
        // was asked about.
        let handlers = object.handlers(req);
        req.frames_mut().with_fake_scope(asked, |req| {
            handlers.read_property(req, object, &name, PropertyRead::Warning)
        })
    }

    /// `ReflectionProperty::setRawValue(object $object, mixed $value)`: the
    /// value stored past the `set` hook of the property the object's class
    /// holds ([`php_ext::call::write_raw_property`], which a virtual
    /// property refuses); with no such hook, `write_property` under
    /// `fake_scope`, `setValue`'s instance arm; a `static` property is
    /// php's `ReflectionException`.
    ///
    /// ```php
    /// class C { public $p = 1; public static $s; } $o = new C;
    /// (new ReflectionProperty('C', 'p'))->setRawValue($o, 5); var_dump($o->p);
    /// try { (new ReflectionProperty('C', 's'))->setRawValue($o, 1); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// class H { public $b { get => $this->b; set => $value * 2; } }
    /// $h = new H;
    /// (new ReflectionProperty('H', 'b'))->setRawValue($h, 7);
    /// var_dump($h->b);
    /// ```
    /// ```expect
    /// int(5)
    /// May not use setRawValue on static properties
    /// int(7)
    /// ```
    fn setRawValue(
        req: &mut Request,
        this: This<Self>,
        object: PhpObject,
        value: Value,
    ) -> Result<(), Thrown> {
        let (class, name, declared) = this_property(req, this.object())?;
        let asked = asked_class(req, this.object(), class);
        let reflected = declared.map(|decl| (class, decl));
        let prop = effective_prop(req, asked, object, reflected, &name);
        if prop.as_ref().is_some_and(|(_, decl)| !decl.is_instance) {
            return Err(throw(
                req,
                "ReflectionException",
                "May not use setRawValue on static properties",
            ));
        }
        set_raw_value(req, asked, prop.as_ref(), object, &name, value)
    }

    /// `ReflectionProperty::setRawValueWithoutLazyInitialization(object $object, mixed $value): void`:
    /// the value written into the property as `setRawValue` writes it, a
    /// `set` hook's trampoline or `write_property` with no `__set`, on the
    /// object the initialised proxies lead to, while the property does not
    /// wait; the object is no longer lazy once none waits.
    ///
    /// ```php
    /// class C { public $a = 1; public int $b = 2; public static $s; public function __set($n, $v) { echo "set $n\n"; } }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function () { echo "init\n"; });
    /// $r->getProperty('a')->setRawValueWithoutLazyInitialization($o, 5);
    /// var_dump($r->isUninitializedLazyObject($o));
    /// try { $r->getProperty('b')->setRawValueWithoutLazyInitialization($o, 'x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($r->getProperty('b')->isLazy($o));
    /// $r->getProperty('b')->setRawValueWithoutLazyInitialization($o, 3);
    /// var_dump($r->isUninitializedLazyObject($o), $o);
    /// try { $r->getProperty('s')->setRawValueWithoutLazyInitialization($o, 1); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// $d = new stdClass; $d->dyn = 1;
    /// try { (new ReflectionProperty($d, 'dyn'))->setRawValueWithoutLazyInitialization($d, 2); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// Cannot assign string to property C::$b of type int
    /// bool(true)
    /// bool(false)
    /// object(C)#3 (2) {
    ///   ["a"]=>
    ///   int(5)
    ///   ["b"]=>
    ///   int(3)
    /// }
    /// Can not use setRawValueWithoutLazyInitialization on static property C::$s
    /// Can not use setRawValueWithoutLazyInitialization on dynamic property stdClass::$dyn
    /// ```
    ///
    /// Over a property with a `set` hook the hook's trampoline is called,
    /// so a throw has its row and the value it hands back is given up as a
    /// possible root:
    ///
    /// ```php
    /// class B {}
    /// class C { public int $n { set { $this->n = $value; } } public B $b { set { $this->b = $value; } } }
    /// $r = new ReflectionClass(C::class);
    /// $g = $r->newLazyGhost(function () { echo "init\n"; });
    /// try { $r->getProperty('n')->setRawValueWithoutLazyInitialization($g, 'x'); } catch (TypeError $e) { echo $e->getTraceAsString(), "\n"; }
    /// var_dump($r->getProperty('n')->isLazy($g));
    /// function t($r) { $g = $r->newLazyGhost(function () {}); $v = new B; gc_collect_cycles(); $r->getProperty('b')->setRawValueWithoutLazyInitialization($g, $v); echo gc_status()['roots'], "\n"; }
    /// t($r);
    /// ```
    /// ```expect
    /// #0 [internal function]: C->$n::set('x')
    /// #1 %s(6): ReflectionProperty->setRawValueWithoutLazyInitialization(Object(C), 'x')
    /// #2 {main}
    /// bool(true)
    /// 2
    /// ```
    ///
    /// The slot is the effective property's, `OBJ_PROP(object, prop->offset)`:
    /// a parent's private property reflected through the parent is that
    /// slot and not the child's of the same name, which still waits, as
    /// it does under `skipLazyInitialization`:
    ///
    /// ```php
    /// class P { private $p = 'P'; public $q = 1; }
    /// class Q extends P { private $p = 'Q'; }
    /// $r = new ReflectionClass(Q::class); $pp = new ReflectionProperty(P::class, 'p'); $qp = new ReflectionProperty(Q::class, 'p');
    /// $g = $r->newLazyGhost(function () { echo "init\n"; });
    /// $pp->setRawValueWithoutLazyInitialization($g, 'X');
    /// var_dump($pp->isLazy($g), $qp->isLazy($g), $r->isUninitializedLazyObject($g));
    /// $h = $r->newLazyGhost(function () { echo "init\n"; });
    /// $pp->skipLazyInitialization($h);
    /// var_dump($pp->isLazy($h), $qp->isLazy($h), $r->isUninitializedLazyObject($h));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// ```
    fn setRawValueWithoutLazyInitialization(
        req: &mut Request,
        this: This<Self>,
        object: PhpObject,
        value: Value,
    ) -> Result<(), Thrown> {
        let (class, name, declared) = this_property(req, this.object())?;
        let asked = asked_class(req, this.object(), class);
        let object = object_of(req, object, asked, 1)?;
        let object = object.lazy_real(req).unwrap_or(object);
        let reflected = declared.map(|decl| (class, decl));
        let prop = effective_prop(req, asked, object, reflected, &name);
        let method = "setRawValueWithoutLazyInitialization";
        lazy_compatible(req, asked, prop.as_ref(), &name, object, method)?;
        if !object.is_lazy() {
            return set_raw_value(req, asked, prop.as_ref(), object, &name, value);
        }
        // `IS_PROP_LAZY` cleared around `reflection_property_set_raw_value`,
        // the write `setRawValue` makes.
        let declaring = prop.as_ref().map_or(class, |(at, _)| *at);
        let mut write = |req: &mut Request, object: PhpObject| {
            set_raw_value(req, asked, prop.as_ref(), object, &name, value)
        };
        skip_lazy_initialization(req, &object, declaring, &name, Some(&mut write))
    }

    /// `ReflectionProperty::skipLazyInitialization(object $object): void`:
    /// the property takes its default and no longer waits; a property that
    /// does not wait is left as it stands.
    ///
    /// ```php
    /// class C { public $a = 1; public int $b = 2; }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function () { echo "init\n"; });
    /// $r->getProperty('a')->skipLazyInitialization($o);
    /// var_dump($o->a, $r->isUninitializedLazyObject($o));
    /// $r->getProperty('b')->skipLazyInitialization($o);
    /// var_dump($r->isUninitializedLazyObject($o), $o);
    /// ```
    /// ```expect
    /// int(1)
    /// bool(true)
    /// bool(false)
    /// object(C)#3 (2) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["b"]=>
    ///   int(2)
    /// }
    /// ```
    fn skipLazyInitialization(
        req: &mut Request,
        this: This<Self>,
        object: PhpObject,
    ) -> Result<(), Thrown> {
        let (class, name, declared) = this_property(req, this.object())?;
        let asked = asked_class(req, this.object(), class);
        let object = object_of(req, object, asked, 1)?;
        let object = object.lazy_real(req).unwrap_or(object);
        let reflected = declared.map(|decl| (class, decl));
        let prop = effective_prop(req, asked, object, reflected, &name);
        let method = "skipLazyInitialization";
        lazy_compatible(req, asked, prop.as_ref(), &name, object, method)?;
        let declaring = prop.as_ref().map_or(class, |(at, _)| *at);
        skip_lazy_initialization(req, &object, declaring, &name, None)?;
        Ok(())
    }

    /// `ReflectionProperty::isLazy(object $object): bool`: whether the
    /// property, on the object the initialised proxies lead to, still waits
    /// for the initializer; `false` for a dynamic property and for a class
    /// property (`ZEND_ACC_STATIC`).
    ///
    /// ```php
    /// class C { public $a = 1; public $b = 2; public static $s; }
    /// $r = new ReflectionClass('C');
    /// $p = $r->newLazyProxy(function () { return new C; });
    /// $r->getProperty('a')->skipLazyInitialization($p);
    /// var_dump($r->getProperty('a')->isLazy($p), $r->getProperty('b')->isLazy($p), $r->getProperty('s')->isLazy($p));
    /// $r->initializeLazyObject($p);
    /// var_dump($r->getProperty('a')->isLazy($p), $r->getProperty('b')->isLazy($p));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// ```
    fn isLazy(req: &mut Request, this: This<Self>, object: PhpObject) -> Result<bool, Thrown> {
        let (class, name, declared) = this_property(req, this.object())?;
        let asked = asked_class(req, this.object(), class);
        let object = object_of(req, object, asked, 1)?;
        let object = object.lazy_real(req).unwrap_or(object);
        let reflected = declared.map(|decl| (class, decl));
        // `reflection_property_get_effective_prop`, then `false` for a
        // static or a virtual property, which has no slot.
        let Some((declaring, decl)) = effective_prop(req, asked, object, reflected, &name) else {
            return Ok(false);
        };
        if !decl.is_instance || php_ext::call::property_is_virtual(req, declaring, &name) {
            return Ok(false);
        }
        Ok(object.is_lazy_prop(req, declaring, &name))
    }

    /// `ReflectionProperty::isInitialized(?object $object = null)`: for a
    /// `static` property, whether its slot holds a value, read silently
    /// through `intern->ce`; for an instance one, whether the object's slot
    /// or dynamic table holds one, the object's own
    /// `has_property(ZEND_PROPERTY_EXISTS)` under `EG(fake_scope) =
    /// intern->ce`, which asks no `__isset` and finds a property the
    /// class's handler serves in place of a slot. The object is held to
    /// the declaring class, php's `ref->prop->ce`, or to `intern->ce` for a
    /// dynamic property.
    ///
    /// ```php
    /// class A { public static ?int $s; public static $t; public ?int $i; public $n; private int $p; }
    /// $a = new A;
    /// var_dump((new ReflectionProperty('A', 's'))->isInitialized(), (new ReflectionProperty('A', 't'))->isInitialized());
    /// var_dump((new ReflectionProperty('A', 'i'))->isInitialized($a), (new ReflectionProperty('A', 'n'))->isInitialized($a), (new ReflectionProperty('A', 'p'))->isInitialized($a));
    /// unset($a->n); var_dump((new ReflectionProperty('A', 'n'))->isInitialized($a));
    /// try { (new ReflectionProperty('A', 'n'))->isInitialized(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { (new ReflectionProperty('A', 'n'))->isInitialized(new stdClass); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// ReflectionProperty::isInitialized(): Argument #1 ($object) must be provided for instance properties
    /// Given object is not an instance of the class this property was declared in
    /// ```
    ///
    /// A property the class's handler serves in place of a slot:
    ///
    /// ```php
    /// $d = new DOMDocument; $d->loadXML('<r/>');
    /// var_dump((new ReflectionProperty('DOMNode', 'nodeType'))->isInitialized($d->documentElement));
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isInitialized(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] object: Option<PhpObject>,
    ) -> Result<bool, Thrown> {
        let (class, name, declared) = this_property(req, this.object())?;
        let asked = asked_class(req, this.object(), class);
        if declared.as_ref().is_some_and(|decl| !decl.is_instance) {
            let Some(value) = read_class_prop_silent(req, asked, &name)? else {
                return Ok(false);
            };
            let held = !matches!(value.as_ref(), ValueRef::Undef);
            value.give_up(req);
            return Ok(held);
        }
        let Some(object) = object else {
            return Err(arg_type_error(
                req,
                1,
                "be provided for instance properties",
            ));
        };
        let holder = if declared.is_some() { class } else { asked };
        held_to(req, object, holder)?;
        let handlers = object.handlers(req);
        req.frames_mut().with_fake_scope(asked, |req| {
            handlers.has_property(req, object, &name, PropertyTest::Exists)
        })
    }

    /// `ReflectionProperty::isPublic()`: `ZEND_ACC_PUBLIC`, which a
    /// dynamic property carries too.
    ///
    /// ```php
    /// var_dump((new ReflectionProperty('Exception', 'message'))->isPublic());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isPublic(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        visibility_is(req, this.object(), Visibility::Public, true)
    }

    /// `ReflectionProperty::isPrivate()`: `ZEND_ACC_PRIVATE`.
    ///
    /// ```php
    /// var_dump((new ReflectionProperty('Exception', 'string'))->isPrivate());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isPrivate(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        visibility_is(req, this.object(), Visibility::Private, false)
    }

    /// `ReflectionProperty::isProtected()`: `ZEND_ACC_PROTECTED`.
    ///
    /// ```php
    /// var_dump((new ReflectionProperty('Exception', 'message'))->isProtected());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isProtected(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        visibility_is(req, this.object(), Visibility::Protected, false)
    }

    /// `ReflectionProperty::isPrivateSet()`: `ZEND_ACC_PRIVATE_SET`, the
    /// set visibility a `private(set)` property carries.
    ///
    /// ```php
    /// class Foo { public private(set) int $bar; }
    /// var_dump((new ReflectionProperty('Foo', 'bar'))->isPrivateSet());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    fn isPrivateSet(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        set_visibility_is(req, this.object(), Visibility::Private)
    }

    /// `ReflectionProperty::isProtectedSet()`: `ZEND_ACC_PROTECTED_SET`.
    ///
    /// ```php
    /// class Foo { public protected(set) int $bar; }
    /// var_dump((new ReflectionProperty('Foo', 'bar'))->isProtectedSet());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    fn isProtectedSet(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        set_visibility_is(req, this.object(), Visibility::Protected)
    }

    /// `ReflectionProperty::isStatic()`: `ZEND_ACC_STATIC`.
    ///
    /// ```php
    /// var_dump((new ReflectionProperty('Exception', 'message'))->isStatic());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isStatic(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, found) = this_declared(req, this.object())?;
        Ok(found.is_some_and(|(_, decl)| !decl.is_instance))
    }

    /// `ReflectionProperty::isReadOnly()`: `ZEND_ACC_READONLY`.
    ///
    /// ```php
    /// var_dump((new ReflectionProperty('Exception', 'message'))->isReadOnly());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    fn isReadOnly(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, found) = this_declared(req, this.object())?;
        Ok(found.is_some_and(|(_, decl)| decl.is_readonly))
    }

    /// `ReflectionProperty::isDefault()`: `_property_check_dynamic`, which
    /// is whether the class declares the property at all.
    ///
    /// ```php
    /// var_dump((new ReflectionProperty('Exception', 'message'))->isDefault());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isDefault(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, found) = this_declared(req, this.object())?;
        Ok(found.is_some())
    }

    /// `ReflectionProperty::isDynamic()`: the other half of
    /// `_property_check_dynamic`.
    ///
    /// ```php
    /// var_dump((new ReflectionProperty('Exception', 'message'))->isDynamic());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    fn isDynamic(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, found) = this_declared(req, this.object())?;
        Ok(found.is_none())
    }

    /// `ReflectionProperty::isAbstract()`: `ZEND_ACC_ABSTRACT`, which a
    /// property carries where declared `abstract` and every interface
    /// property carries.
    ///
    /// ```php
    /// interface I { public $i { get; } }
    /// abstract class A { abstract public $a { get; } public $p; }
    /// var_dump((new ReflectionProperty('I', 'i'))->isAbstract(), (new ReflectionProperty('A', 'a'))->isAbstract(), (new ReflectionProperty('A', 'p'))->isAbstract());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// ```
    fn isAbstract(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, found) = this_declared(req, this.object())?;
        Ok(found
            .is_some_and(|(_, decl)| decl.hooks.as_ref().is_some_and(|hooks| hooks.is_abstract)))
    }

    /// `ReflectionProperty::isVirtual()`: `ZEND_ACC_VIRTUAL`, a hooked
    /// property no declaration of which up the chain has a backing value
    /// ([`php_ext::call::property_is_virtual`]).
    ///
    /// ```php
    /// class C { public $v { get => 1; } public $b { get => $this->b; } }
    /// var_dump((new ReflectionProperty('C', 'v'))->isVirtual(), (new ReflectionProperty('C', 'b'))->isVirtual());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    fn isVirtual(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, name, found) = this_declared(req, this.object())?;
        Ok(found.is_some_and(|(declaring, _)| property_is_virtual(req, declaring, &name)))
    }

    /// `ReflectionProperty::isPromoted()`: `ZEND_ACC_PROMOTED`, the bit on
    /// the property a promoted constructor parameter declares; a dynamic
    /// property has no info and answers `false`.
    ///
    /// ```php
    /// class P { function __construct(public int $x, /** D */ public $z, $y) {} public $w; }
    /// var_dump((new ReflectionProperty('P', 'x'))->isPromoted(), (new ReflectionProperty('P', 'w'))->isPromoted(), (new ReflectionProperty('Exception', 'message'))->isPromoted());
    /// var_dump((new ReflectionProperty('P', 'z'))->getDocComment());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// string(8) "/** D */"
    /// ```
    fn isPromoted(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, found) = this_declared(req, this.object())?;
        Ok(found.is_some_and(|(_, decl)| decl.is_promoted))
    }

    /// `ReflectionProperty::getModifiers()`: `prop_get_flags(ref)` kept to
    /// the `PPP | PPP_SET | STATIC | READONLY | ABSTRACT | VIRTUAL |
    /// FINAL` mask. A dynamic property has no info, and `prop_get_flags`
    /// answers `ZEND_ACC_PUBLIC` for it.
    ///
    /// ```php
    /// $o = new stdClass; $o->d = 1;
    /// abstract class A { abstract public $a { get; } public $v { get => 1; } }
    /// var_dump((new ReflectionProperty('Exception', 'message'))->getModifiers(), (new ReflectionProperty($o, 'd'))->getModifiers());
    /// var_dump((new ReflectionProperty('A', 'a'))->getModifiers(), (new ReflectionProperty('A', 'v'))->getModifiers());
    /// ```
    /// ```expect
    /// int(2)
    /// int(1)
    /// int(577)
    /// int(513)
    /// ```
    #[php(tentative)]
    fn getModifiers(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let (_, name, found) = this_declared(req, this.object())?;
        let Some((declaring, decl)) = found else {
            return Ok(1);
        };
        Ok(property_flags(req, declaring, &name, &decl))
    }

    /// `ReflectionProperty::getDeclaringClass()`:
    /// `zend_reflection_class_factory` over `prop->ce`, and the class the
    /// reflector names for a dynamic property.
    ///
    /// ```php
    /// var_dump((new ReflectionProperty('JsonException', 'message'))->getDeclaringClass()->getName());
    /// ```
    /// ```expect
    /// string(9) "Exception"
    /// ```
    #[php(tentative)]
    fn getDeclaringClass(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<crate::class::ReflectionClass>>, Thrown> {
        let (class, _, found) = this_declared(req, this.object())?;
        let scope = found.map_or(class, |(scope, _)| scope);
        let object = reflection_class_factory(req, scope)?;
        Ok(object.into_instance())
    }

    /// `ReflectionProperty::getDocComment()`: `prop->doc_comment`, or
    /// `false` where the declaration carries none and where there is no
    /// declaration at all, php's `ref->prop && ref->prop->doc_comment`, a
    /// dynamic property being the second case. A declarator takes the
    /// comment that stands before the declaration only when it is the
    /// first, `backup_doc_comment` reducing at each `property:`, so
    /// `/** P */ public $x, $y;` gives it to `$x` alone.
    ///
    /// ```php
    /// class M { /** P */ public $x, $y; } var_dump((new ReflectionProperty('M', 'x'))->getDocComment(), (new ReflectionProperty('M', 'y'))->getDocComment(), (new ReflectionProperty('Exception', 'message'))->getDocComment());
    /// ```
    /// ```expect
    /// string(8) "/** P */"
    /// bool(false)
    /// bool(false)
    /// ```
    ///
    /// ```php
    /// class M { public $x; } (new ReflectionProperty('M', 'x'))->getDocComment(1);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: ReflectionProperty::getDocComment() expects exactly 0 arguments, 1 given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionProperty->getDocComment(1)
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    #[php(tentative)]
    fn getDocComment(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let (_, _, declared) = this_property(req, this.object())?;
        match declared.and_then(|decl| decl.doc_comment) {
            Some(doc) => PhpString::new(req, &doc).map(OrFalse::Value),
            None => Ok(OrFalse::False),
        }
    }

    #[php(tentative, deprecated(since = "8.5", message = "as it has no effect since PHP 8.1"), implementation = crate::func::ReflectionMethod::setAccessible)]
    fn setAccessible(this: This<Self>, accessible: bool);

    /// `ReflectionProperty::getType()`: `reflection_type_factory` over
    /// `prop->type` with `legacy_behavior` set, and `null` where the
    /// property is untyped or dynamic.
    ///
    /// ```php
    /// echo (new ReflectionProperty('ReflectionClass', 'name'))->getType(), "\n";
    /// ```
    /// ```expect
    /// string
    /// ```
    #[php(tentative)]
    fn getType(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::signature::ReflectionType>>>, Thrown> {
        let (_, _, found) = this_declared(req, this.object())?;
        let Some(ty) = found.and_then(|(_, decl)| decl.ty) else {
            return Ok(None);
        };
        let object = type_factory(req, &ty, true)?;
        Ok(Some(object.into_instance()))
    }

    /// `ReflectionProperty::getSettableType()`: the type a write must
    /// satisfy: `never` for a virtual property with no `set` hook, which no
    /// write reaches; the `set` hook's parameter's where it has one, none
    /// where that is untyped; and `getType()`'s otherwise.
    ///
    /// ```php
    /// class C { public int $v { get => 1; } public int $s { set(int|string $x) { } } public int $p; }
    /// foreach (['v', 's', 'p'] as $n) echo (new ReflectionProperty('C', $n))->getSettableType(), "\n";
    /// echo (new ReflectionProperty('ReflectionClass', 'name'))->getSettableType(), "\n";
    /// ```
    /// ```expect
    /// never
    /// string|int
    /// int
    /// string
    /// ```
    fn getSettableType(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::signature::ReflectionType>>>, Thrown> {
        let (_, name, found) = this_declared(req, this.object())?;
        let Some((declaring, decl)) = &found else {
            return Ok(None);
        };
        let set = hook_of(req, found.as_ref(), &name, HookKind::Set);
        let hooked = set.is_some() || hook_of(req, found.as_ref(), &name, HookKind::Get).is_some();
        let ty = match set {
            None if hooked && property_is_virtual(req, *declaring, &name) => Some(Type::Never),
            Some((_, set)) => set
                .function
                .parameters
                .first()
                .and_then(|parameter| parameter.ty.clone()),
            None => decl.ty.clone(),
        };
        let Some(ty) = ty else {
            return Ok(None);
        };
        Ok(Some(type_factory(req, &ty, true)?.into_instance()))
    }

    /// `ReflectionProperty::hasType()`: `ZEND_TYPE_IS_SET(prop->type)`.
    ///
    /// ```php
    /// var_dump((new ReflectionProperty('ReflectionClass', 'name'))->hasType());
    /// var_dump((new ReflectionProperty('Exception', 'message'))->hasType());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn hasType(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, found) = this_declared(req, this.object())?;
        Ok(found.is_some_and(|(_, decl)| decl.ty.is_some()))
    }

    /// `ReflectionProperty::hasDefaultValue()`: whether
    /// `property_get_default` gives a value that is not `UNDEF`; an
    /// untyped property with nothing written has php's implicit `null` and
    /// a typed one has none, and a virtual property has no slot to hold
    /// one.
    ///
    /// ```php
    /// class C { public $v { get => 1; } public $b { get => $this->b; } }
    /// var_dump((new ReflectionProperty('Exception', 'message'))->hasDefaultValue());
    /// var_dump((new ReflectionProperty('ReflectionClass', 'name'))->hasDefaultValue());
    /// var_dump((new ReflectionProperty('C', 'v'))->hasDefaultValue(), (new ReflectionProperty('C', 'b'))->hasDefaultValue());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// ```
    fn hasDefaultValue(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, name, found) = this_declared(req, this.object())?;
        let default = default_of(req, &name, found);
        Ok(!matches!(default, Default::Absent))
    }

    /// `ReflectionProperty::getDefaultValue()`: the default expression
    /// evaluated in the declaring class, php's implicit `null` for an
    /// untyped property with nothing written, and php's own
    /// `E_DEPRECATED` and `null` where there is no default at all, a
    /// dynamic property among them, which has no info to hold one.
    ///
    /// ```php
    /// var_dump((new ReflectionProperty('Exception', 'message'))->getDefaultValue());
    /// ```
    /// ```expect
    /// string(0) ""
    /// ```
    ///
    /// ```php
    /// var_dump((new ReflectionProperty('ReflectionClass', 'name'))->getDefaultValue());
    /// ```
    /// ```expect
    /// Deprecated: ReflectionProperty::getDefaultValue() for a property without a default value is deprecated, use ReflectionProperty::hasDefaultValue() to check if the default value exists in %s on line 2
    /// NULL
    /// ```
    ///
    /// ```php
    /// $o = new stdClass; $o->d = 1;
    /// var_dump((new ReflectionProperty($o, 'd'))->getDefaultValue());
    /// ```
    /// ```expect
    /// Deprecated: ReflectionProperty::getDefaultValue() for a property without a default value is deprecated, use ReflectionProperty::hasDefaultValue() to check if the default value exists in %s on line 3
    /// NULL
    /// ```
    #[php(tentative)]
    fn getDefaultValue(req: &mut Request, this: This<Self>) -> Result<Owner<Value>, Thrown> {
        let (_, name, found) = this_declared(req, this.object())?;
        match default_of(req, &name, found) {
            Default::Written(expr, scope) => eval_declared(req, &expr, Some(scope)),
            Default::ImplicitNull => Ok(Owner::null()),
            Default::Absent => {
                deprecated(
                    req,
                    "ReflectionProperty::getDefaultValue() for a property without a default value is deprecated, use ReflectionProperty::hasDefaultValue() to check if the default value exists",
                );
                Ok(Owner::null())
            }
        }
    }

    /// `ReflectionProperty::getAttributes(?string $name = null, int $flags
    /// = 0)`: `reflect_attributes` over `prop->attributes` with
    /// `ZEND_ATTRIBUTE_TARGET_PROPERTY`.
    ///
    /// ```php
    /// var_dump((new ReflectionProperty('Exception', 'message'))->getAttributes());
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
        let (_, _, found) = this_declared(req, this.object())?;
        // `ref->prop == NULL`, a dynamic property: no attributes, before
        // the filter is looked at.
        let Some((scope, decl)) = found else {
            return Ok(Owner::take(req, PhpArray::empty()));
        };
        let file = req
            .symbols()
            .class_decl(scope)
            .and_then(|decl| super::attr::declared_in(&decl.file.0));
        super::attr::reflect_attributes(
            req,
            &decl.attributes,
            super::attr::Subject::new(Some(scope), super::attr::TARGET_PROPERTY, file.as_deref()),
            name,
            flags,
        )
    }

    /// `ReflectionProperty::hasHooks()`: `ref->prop && ref->prop->hooks`,
    /// the property's own hooks or ones it inherits.
    ///
    /// ```php
    /// class C { public $p; public $h { get => 1; } } $o = new C; $o->d = 1;
    /// class D extends C { public $h; }
    /// var_dump((new ReflectionProperty('C', 'p'))->hasHooks(), (new ReflectionProperty($o, 'd'))->hasHooks());
    /// var_dump((new ReflectionProperty('C', 'h'))->hasHooks(), (new ReflectionProperty('D', 'h'))->hasHooks());
    /// ```
    /// ```expect
    /// Deprecated: Creation of dynamic property C::$d is deprecated in %s on line 2
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// ```
    fn hasHooks(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, name, found) = this_declared(req, this.object())?;
        Ok(HookKind::ALL
            .into_iter()
            .any(|kind| hook_of(req, found.as_ref(), &name, kind).is_some()))
    }

    /// `ReflectionProperty::getHooks()`: the `get` and `set` hooks keyed by
    /// kind, each a `ReflectionMethod`, and the empty array for a property
    /// without hooks.
    ///
    /// ```php
    /// class C { public $p; public $h { set {} } }
    /// var_dump((new ReflectionProperty('C', 'p'))->getHooks(), (new ReflectionProperty('C', 'h'))->getHooks());
    /// ```
    /// ```expect
    /// array(0) {
    /// }
    /// array(1) {
    ///   ["set"]=>
    ///   object(ReflectionMethod)#%d (2) {
    ///     ["name"]=>
    ///     string(7) "$h::set"
    ///     ["class"]=>
    ///     string(1) "C"
    ///   }
    /// }
    /// ```
    fn getHooks(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let (_, name, found) = this_declared(req, this.object())?;
        let declared: Vec<_> = HookKind::ALL
            .into_iter()
            .filter_map(|kind| Some((kind, hook_of(req, found.as_ref(), &name, kind)?)))
            .collect();
        if declared.is_empty() {
            return Ok(Owner::take(req, PhpArray::empty()));
        }
        let mut hooks = PhpArray::new(req)?;
        for (kind, (scope, decl)) in declared {
            let reflector = hook_reflector(req, scope, &decl)?;
            hooks.set_str(req, kind.name().as_bytes(), reflector.into_value())?;
        }
        Ok(hooks)
    }

    /// `ReflectionProperty::hasHook(PropertyHookType $type)`: whether the
    /// property's hooks include one of that kind.
    ///
    /// ```php
    /// class C { public $p; public $h { get => 1; } } $r = new ReflectionProperty('C', 'p');
    /// var_dump($r->hasHook(PropertyHookType::Get), $r->hasHook(PropertyHookType::Set));
    /// $h = new ReflectionProperty('C', 'h');
    /// var_dump($h->hasHook(PropertyHookType::Get), $h->hasHook(PropertyHookType::Set));
    /// try { $r->hasHook('get'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// ReflectionProperty::hasHook(): Argument #1 ($type) must be of type PropertyHookType, string given
    /// ```
    fn hasHook(
        req: &mut Request,
        this: This<Self>,
        r#type: PropertyHookType,
    ) -> Result<bool, Thrown> {
        let kind = r#type.kind();
        let (_, name, found) = this_declared(req, this.object())?;
        Ok(hook_of(req, found.as_ref(), &name, kind).is_some())
    }

    /// `ReflectionProperty::getHook(PropertyHookType $type)`: a
    /// `ReflectionMethod` over the property's hook of that kind, `null`
    /// where it has none; the method reflected is the hook itself, which
    /// `invoke` runs.
    ///
    /// ```php
    /// class C { public $p; public $h = 1 { get => $this->h * 10; } }
    /// var_dump((new ReflectionProperty('C', 'p'))->getHook(PropertyHookType::Get));
    /// $g = (new ReflectionProperty('C', 'h'))->getHook(PropertyHookType::Get);
    /// var_dump($g->getName(), $g->invoke(new C), $g->getNumberOfParameters());
    /// ```
    /// ```expect
    /// NULL
    /// string(7) "$h::get"
    /// int(10)
    /// int(0)
    /// ```
    fn getHook(
        req: &mut Request,
        this: This<Self>,
        r#type: PropertyHookType,
    ) -> Result<Option<Owner<Instance<crate::func::ReflectionMethod>>>, Thrown> {
        let kind = r#type.kind();
        let (_, name, found) = this_declared(req, this.object())?;
        match hook_of(req, found.as_ref(), &name, kind) {
            Some((scope, decl)) => Ok(Some(hook_reflector(req, scope, &decl)?.into_instance())),
            None => Ok(None),
        }
    }

    /// `ReflectionProperty::isFinal()`: `ZEND_ACC_FINAL`, written or given
    /// by a `private(set)`.
    ///
    /// ```php
    /// class C { final public $f; public private(set) int $p; public $n; }
    /// foreach (['f', 'p', 'n'] as $x) var_dump((new ReflectionProperty('C', $x))->isFinal());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// ```
    fn isFinal(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, found) = this_declared(req, this.object())?;
        Ok(found.is_some_and(|(_, decl)| decl.is_final))
    }
}

/// `ReflectionClassConstant` as the stub declares it.
#[php_class(implements(crate::Reflector), not_serializable, size = 96)]
pub(crate) struct ReflectionClassConstant;

#[php_class]
impl ReflectionClassConstant {
    const IS_PUBLIC: i64 = 1;
    const IS_PROTECTED: i64 = 2;
    const IS_PRIVATE: i64 = 4;
    const IS_FINAL: i64 = 32;
    #[php(property)]
    const name: PhpString;
    #[php(property)]
    const class: PhpString;

    #[php(private, implementation = crate::func::ReflectionFunctionAbstract::__clone)]
    fn __clone(this: This<Self>);

    /// `ReflectionClassConstant::__construct(object|string $class, string
    /// $constant)`: the constant `zend_hash_find_ptr(CE_CONSTANTS_TABLE(ce),
    /// constname)` finds, else php's `ReflectionException`; `$name` is the
    /// name as given and `$class` is `constant->ce->name`, the class that
    /// declared it, which for an inherited constant is the parent. A
    /// parent's private constant is not in the table and so is a miss.
    ///
    /// ```php
    /// class M { /** K */ const A = 1, B = 2; } var_dump(new ReflectionClassConstant('M', 'A'), (new ReflectionClassConstant('ReflectionProperty', 'IS_PUBLIC'))->class);
    /// ```
    /// ```expect
    /// object(ReflectionClassConstant)#1 (2) {
    ///   ["name"]=>
    ///   string(1) "A"
    ///   ["class"]=>
    ///   string(1) "M"
    /// }
    /// string(18) "ReflectionProperty"
    /// ```
    ///
    /// A parent's private constant is not in the child's table, so the
    /// child is not where it is found (#257, #388):
    ///
    /// ```php
    /// class P { private const R = 2; }
    /// class C extends P {}
    /// new ReflectionClassConstant('C', 'R');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Constant C::R does not exist in %s:4
    /// Stack trace:
    /// #0 %s(4): ReflectionClassConstant->__construct('C', 'R')
    /// #1 {main}
    ///   thrown in %s on line 4
    /// ```
    ///
    /// ```php
    /// class M { const A = 1; } new ReflectionClassConstant('M', 'Z');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Constant M::Z does not exist in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClassConstant->__construct('M', 'Z')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// new ReflectionClassConstant('Nope', 'A');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Class "Nope" does not exist in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClassConstant->__construct('Nope', 'A')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// new ReflectionClassConstant('Exception');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: ReflectionClassConstant::__construct() expects exactly 2 arguments, 1 given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionClassConstant->__construct('Exception')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        class: Union<PhpObject, PhpString>,
        constant: PhpString,
    ) -> Result<(), Thrown> {
        let (class, _) = class_of(req, class)?;
        let name = constant.as_bytes().to_vec();
        let Some((declaring, _)) = class_constant_named(req, class, &name) else {
            let class = class_name_of(req, class);
            return Err(throw(
                req,
                "ReflectionException",
                &format!(
                    "Constant {}::{} does not exist",
                    String::from_utf8_lossy(&class),
                    String::from_utf8_lossy(&name)
                ),
            ));
        };
        let declaring = class_name_of(req, declaring);
        let this = this.object();
        set_name(req, this, &name)?;
        set_string_prop(req, this, b"class", &declaring)?;
        Ok(())
    }

    /// `ReflectionClassConstant::__toString()`: `_class_const_string`, the
    /// row `_class_string` prints inside its `Constants` block.
    ///
    /// ```php
    /// echo new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED');
    /// echo new ReflectionClassConstant('PropertyHookType', 'Get');
    /// ```
    /// ```expect
    /// Constant [ public int IS_DEPRECATED ] { 2048 }
    /// Constant [ public PropertyHookType Get ] { Object }
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let (_, scope, decl) = this_constant(req, this.object())?;
        let held = class_name_of(req, scope);
        let enum_of = req
            .symbols()
            .class_decl(scope)
            .is_some_and(|decl| decl.kind == ClassKind::Enum)
            .then_some(held.as_slice());
        let text = class_const_string(req, &decl, scope, enum_of, "")?;
        PhpString::new(req, text.as_bytes())
    }

    /// `ReflectionClassConstant::getName()`: the constant's name.
    ///
    /// ```php
    /// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->getName());
    /// ```
    /// ```expect
    /// string(13) "IS_DEPRECATED"
    /// ```
    #[php(tentative)]
    fn getName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let (name, _, _) = this_constant(req, this.object())?;
        PhpString::declared(req, &name)
    }

    /// `ReflectionClassConstant::getValue()`: the constant expression
    /// evaluated in its declaring class, `zend_update_class_constant`.
    ///
    /// ```php
    /// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->getValue());
    /// ```
    /// ```expect
    /// int(2048)
    /// ```
    #[php(tentative)]
    fn getValue(req: &mut Request, this: This<Self>) -> Result<Owner<Value>, Thrown> {
        let (_, scope, decl) = this_constant(req, this.object())?;
        super::constant_value(req, &decl, scope)
    }

    /// `ReflectionClassConstant::isPublic()`.
    ///
    /// ```php
    /// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->isPublic());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn isPublic(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        constant_visibility_is(req, this.object(), Visibility::Public)
    }

    /// `ReflectionClassConstant::isPrivate()`.
    ///
    /// ```php
    /// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->isPrivate());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isPrivate(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        constant_visibility_is(req, this.object(), Visibility::Private)
    }

    /// `ReflectionClassConstant::isProtected()`.
    ///
    /// ```php
    /// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->isProtected());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isProtected(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        constant_visibility_is(req, this.object(), Visibility::Protected)
    }

    /// `ReflectionClassConstant::isFinal()`: `ZEND_ACC_FINAL`.
    ///
    /// ```php
    /// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->isFinal());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    fn isFinal(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, decl) = this_constant(req, this.object())?;
        Ok(decl.is_final)
    }

    /// `ReflectionClassConstant::getModifiers()`: the visibility and
    /// `ZEND_ACC_FINAL` bits of `ZEND_CLASS_CONST_FLAGS`.
    ///
    /// ```php
    /// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->getModifiers());
    /// ```
    /// ```expect
    /// int(1)
    /// ```
    #[php(tentative)]
    fn getModifiers(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let (_, _, decl) = this_constant(req, this.object())?;
        let mut bits = match decl.visibility {
            Visibility::Public => 1,
            Visibility::Protected => 2,
            Visibility::Private => 4,
        };
        if decl.is_final {
            bits |= 32;
        }
        Ok(bits)
    }

    /// `ReflectionClassConstant::getDeclaringClass()`:
    /// `zend_reflection_class_factory` over `constant->ce`.
    ///
    /// ```php
    /// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->getDeclaringClass()->getName());
    /// ```
    /// ```expect
    /// string(18) "ReflectionFunction"
    /// ```
    #[php(tentative)]
    fn getDeclaringClass(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<crate::class::ReflectionClass>>, Thrown> {
        let (_, scope, _) = this_constant(req, this.object())?;
        let object = reflection_class_factory(req, scope)?;
        Ok(object.into_instance())
    }

    /// `ReflectionClassConstant::getDocComment()`: `ref->doc_comment`, or
    /// `false` where the declaration carries none. The constant
    /// declarators take the comment as the property declarators do, the
    /// first alone.
    ///
    /// ```php
    /// class M { /** K */ const A = 1, B = 2; } var_dump((new ReflectionClassConstant('M', 'A'))->getDocComment(), (new ReflectionClassConstant('M', 'B'))->getDocComment(), (new ReflectionClassConstant('ReflectionProperty', 'IS_PUBLIC'))->getDocComment());
    /// ```
    /// ```expect
    /// string(8) "/** K */"
    /// bool(false)
    /// bool(false)
    /// ```
    ///
    /// ```php
    /// var_dump((new ReflectionClassConstant('ReflectionObject', 'IS_FINAL'))->class, (new ReflectionClassConstant('ReflectionObject', 'IS_FINAL'))->getDocComment());
    /// ```
    /// ```expect
    /// string(15) "ReflectionClass"
    /// bool(false)
    /// ```
    ///
    /// An inherited constant is the parent's declaration, so the comment is
    /// the parent's and `$class` is the parent (#257, #388):
    ///
    /// ```php
    /// class P { /** R */ protected const R = 2; }
    /// class C extends P {}
    /// $r = new ReflectionClassConstant('C', 'R');
    /// var_dump($r->class, $r->getDocComment());
    /// ```
    /// ```expect
    /// string(1) "P"
    /// string(8) "/** R */"
    /// ```
    #[php(tentative)]
    fn getDocComment(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        match this_class_constant(req, this.object())?.and_then(|decl| decl.doc_comment) {
            Some(doc) => PhpString::new(req, &doc).map(OrFalse::Value),
            None => Ok(OrFalse::False),
        }
    }

    /// `ReflectionClassConstant::getAttributes(?string $name = null, int
    /// $flags = 0)`: `reflect_attributes` over the constant's attributes
    /// with `ZEND_ATTRIBUTE_TARGET_CLASS_CONST`.
    ///
    /// ```php
    /// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->getAttributes());
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
        let (_, scope, decl) = this_constant(req, this.object())?;
        let attributes = decl.attributes.clone();
        let file = req
            .symbols()
            .class_decl(scope)
            .and_then(|decl| super::attr::declared_in(&decl.file.0));
        super::attr::reflect_attributes(
            req,
            &attributes,
            super::attr::Subject::new(
                Some(scope),
                super::attr::TARGET_CLASS_CONST,
                file.as_deref(),
            ),
            name,
            flags,
        )
    }

    /// `ReflectionClassConstant::isEnumCase()`: `ZEND_CLASS_CONST_IS_CASE`,
    /// which every constant of an enum carries and no other does.
    ///
    /// ```php
    /// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->isEnumCase());
    /// var_dump((new ReflectionClassConstant('PropertyHookType', 'Get'))->isEnumCase());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// ```
    fn isEnumCase(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, decl) = this_constant(req, this.object())?;
        Ok(decl.is_case)
    }

    /// `ReflectionClassConstant::isDeprecated()`:
    /// `ZEND_ACC_DEPRECATED`, which the `#[\Deprecated]` attribute sets,
    /// read from the attribute as `_function_string` reads a function's.
    ///
    /// ```php
    /// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->isDeprecated());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    fn isDeprecated(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, decl) = this_constant(req, this.object())?;
        Ok(decl
            .attributes
            .iter()
            .any(|a| a.name.0.eq_ignore_ascii_case(b"Deprecated")))
    }

    /// `ReflectionClassConstant::hasType()`: whether the constant was
    /// declared with a type, which php 8.3 allows.
    ///
    /// ```php
    /// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->hasType());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    fn hasType(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (_, _, decl) = this_constant(req, this.object())?;
        Ok(decl.ty.is_some())
    }

    /// `ReflectionClassConstant::getType()`: `reflection_type_factory` over
    /// the declared type, and `null` where the constant has none.
    ///
    /// ```php
    /// echo (new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->getType(), "\n";
    /// ```
    /// ```expect
    /// int
    /// ```
    fn getType(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::signature::ReflectionType>>>, Thrown> {
        let (_, _, decl) = this_constant(req, this.object())?;
        let Some(ty) = decl.ty else {
            return Ok(None);
        };
        let object = type_factory(req, &ty, true)?;
        Ok(Some(object.into_instance()))
    }
}

/// `ReflectionConstant` as the stub declares it, `@strict-properties`
/// among its flags: a write of an undeclared property is php's `Error`.
///
/// ```php
/// $r = new ReflectionConstant("PHP_VERSION");
/// try { $r->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump(isset($r->x));
/// ```
/// ```expect
/// Cannot create dynamic property ReflectionConstant::$x
/// bool(false)
/// ```
#[php_class(implements(crate::Reflector), not_serializable, size = 96, dynamic_properties = forbidden)]
pub(crate) struct ReflectionConstant;

#[php_class]
impl ReflectionConstant {
    #[php(property)]
    const name: PhpString;

    /// `ReflectionConstant::__construct(string $name)`: the constant under
    /// the name with its namespace lowered and a leading `\` dropped,
    /// `zend_get_constant_ptr`; a miss is php's `ReflectionException` with the name as
    /// given, and `$name` keeps the spelling given too.
    ///
    /// ```php
    /// var_dump(new ReflectionConstant('\PHP_VERSION'), (new ReflectionConstant('PHP_EOL'))->name);
    /// ```
    /// ```expect
    /// object(ReflectionConstant)#1 (1) {
    ///   ["name"]=>
    ///   string(12) "\PHP_VERSION"
    /// }
    /// string(7) "PHP_EOL"
    /// ```
    ///
    /// ```php
    /// new ReflectionConstant('NOPE');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ReflectionException: Constant "NOPE" does not exist in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionConstant->__construct('NOPE')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// new ReflectionConstant();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: ReflectionConstant::__construct() expects exactly 1 argument, 0 given in %s:2
    /// Stack trace:
    /// #0 %s(2): ReflectionConstant->__construct()
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    fn __construct(req: &mut Request, this: This<Self>, name: PhpString) -> Result<(), Thrown> {
        let name = name.as_bytes().to_vec();
        let lowered = constant_key(&name);
        if constant(req, &lowered)?.is_none() {
            return Err(throw(
                req,
                "ReflectionException",
                &format!(
                    "Constant \"{}\" does not exist",
                    String::from_utf8_lossy(&name)
                ),
            ));
        }
        let this = this.object();
        set_name(req, this, &name)?;
        Ok(())
    }

    /// `ReflectionConstant::getName()`: `const_->name`.
    ///
    /// ```php
    /// var_dump((new ReflectionConstant('PHP_EOL'))->getName());
    /// ```
    /// ```expect
    /// string(7) "PHP_EOL"
    /// ```
    fn getName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let name = this_global_constant(req, this.object())?;
        PhpString::declared(req, &name)
    }

    /// `ReflectionConstant::getNamespaceName()`: the name before the last
    /// `\`, and the empty string where there is none.
    ///
    /// ```php
    /// var_dump((new ReflectionConstant('PHP_EOL'))->getNamespaceName());
    /// ```
    /// ```expect
    /// string(0) ""
    /// ```
    fn getNamespaceName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let name = this_global_constant(req, this.object())?;
        let space = namespace_end(&name).map_or_else(Vec::new, |end| name[..end].to_vec());
        PhpString::new(req, &space)
    }

    /// `ReflectionConstant::getShortName()`: the name after the last `\`.
    ///
    /// ```php
    /// var_dump((new ReflectionConstant('PHP_EOL'))->getShortName());
    /// ```
    /// ```expect
    /// string(7) "PHP_EOL"
    /// ```
    fn getShortName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let name = this_global_constant(req, this.object())?;
        let start = namespace_end(&name).map_or(0, |end| end + 1);
        let short = name[start..].to_vec();
        PhpString::new(req, &short)
    }

    /// `ReflectionConstant::getValue()`: `const_->value`.
    ///
    /// ```php
    /// var_dump((new ReflectionConstant('PHP_INT_SIZE'))->getValue());
    /// ```
    /// ```expect
    /// int(8)
    /// ```
    fn getValue(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let name = this_global_constant(req, this.object())?;
        Ok(constant(req, &name)?.unwrap_or_default())
    }

    /// `ReflectionConstant::isDeprecated()`: `CONST_DEPRECATED`, which is
    /// the `#[\Deprecated]` attribute on the declaration, and `E_STRICT`
    /// is the one constant carrying it.
    ///
    /// ```php
    /// var_dump((new ReflectionConstant('E_STRICT'))->isDeprecated());
    /// var_dump((new ReflectionConstant('PHP_EOL'))->isDeprecated());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    fn isDeprecated(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let name = this_global_constant(req, this.object())?;
        Ok(super::constant_is_deprecated(req, &name))
    }

    /// `ReflectionConstant::getFileName()`: `const_->filename`, the file
    /// executing when `define` or `const` wrote the constant, and `false`
    /// for one an extension or the engine registered.
    ///
    /// ```php
    /// define('D', 1);
    /// var_dump(basename((new ReflectionConstant('D'))->getFileName()), (new ReflectionConstant('PHP_VERSION'))->getFileName(), (new ReflectionConstant('STDIN'))->getFileName());
    /// ```
    /// ```expect
    /// string(11) "witness.php"
    /// bool(false)
    /// bool(false)
    /// ```
    fn getFileName(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let name = this_global_constant(req, this.object())?;
        match req.symbols().constant_file(&name).map(<[u8]>::to_vec) {
            Some(file) => PhpString::new(req, &file).map(OrFalse::Value),
            None => Ok(OrFalse::False),
        }
    }

    /// `ReflectionConstant::getExtension()`: the same name as a
    /// `ReflectionExtension`, and `null` where the name is `false`.
    ///
    /// ```php
    /// var_dump((new ReflectionConstant('JSON_ERROR_NONE'))->getExtension());
    /// ```
    /// ```expect
    /// object(ReflectionExtension)#2 (1) {
    ///   ["name"]=>
    ///   string(4) "json"
    /// }
    /// ```
    fn getExtension(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::ReflectionExtension>>>, Thrown> {
        let Some(name) = constant_extension_name(req, this.object())? else {
            return Ok(None);
        };
        let class = class_named(req, b"ReflectionExtension")?;
        let object = super::new_object(req, class)?;
        super::set_name(req, *object, name.as_bytes())?;
        Ok(Some(object.into_instance()))
    }

    /// `ReflectionConstant::getExtensionName()`: the extension that
    /// declared the constant, and `false` for one a program defined.
    ///
    /// ```php
    /// var_dump((new ReflectionConstant('JSON_ERROR_NONE'))->getExtensionName());
    /// ```
    /// ```expect
    /// string(4) "json"
    /// ```
    fn getExtensionName(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let Some(name) = constant_extension_name(req, this.object())? else {
            return Ok(OrFalse::False);
        };
        PhpString::new(req, name.as_bytes()).map(OrFalse::Value)
    }

    /// `ReflectionConstant::__toString()`: `_const_string` over the name
    /// and the value, the row `_extension_string` prints inside its
    /// `Constants` block.
    ///
    /// ```php
    /// echo new ReflectionConstant('PHP_INT_SIZE');
    /// echo new ReflectionConstant('E_STRICT');
    /// ```
    /// ```expect
    /// Constant [ <persistent> int PHP_INT_SIZE ] { 8 }
    /// Constant [ <persistent, deprecated> int E_STRICT ] { 2048 }
    /// ```
    ///
    /// ```php
    /// define('DEMO', 1);
    /// echo new ReflectionConstant('DEMO');
    /// ```
    /// ```expect
    /// Constant [ int DEMO ] { 1 }
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let name = this_global_constant(req, this.object())?;
        let text = const_string(req, &name, "");
        PhpString::new(req, text.as_bytes())
    }

    /// `ReflectionConstant::getAttributes(?string $name = null, int $flags
    /// = 0)`: `reflect_attributes` over `const_->attributes`, the
    /// declaration's own, at `ZEND_ATTRIBUTE_TARGET_CONST` and in no
    /// scope, since a global constant's arguments are evaluated outside
    /// every class. `E_STRICT` is the one constant carrying an attribute
    /// here, as it is the one carrying one in php; a constant a program
    /// defined carries none until the compiler reads the `#[...]` written
    /// on a `const` (#465).
    ///
    /// ```php
    /// var_dump((new ReflectionConstant('E_STRICT'))->getAttributes());
    /// ```
    /// ```expect
    /// array(1) {
    ///   [0]=>
    ///   object(ReflectionAttribute)#2 (1) {
    ///     ["name"]=>
    ///     string(10) "Deprecated"
    ///   }
    /// }
    /// ```
    ///
    /// ```php
    /// var_dump((new ReflectionConstant('E_STRICT'))->getAttributes()[0]->getArguments());
    /// var_dump((new ReflectionConstant('PHP_EOL'))->getAttributes());
    /// var_dump(count((new ReflectionConstant('E_STRICT'))->getAttributes('Deprecated')), count((new ReflectionConstant('E_STRICT'))->getAttributes('Other')));
    /// ```
    /// ```expect
    /// array(2) {
    ///   ["since"]=>
    ///   string(3) "8.4"
    ///   ["message"]=>
    ///   string(27) "the error level was removed"
    /// }
    /// array(0) {
    /// }
    /// int(1)
    /// int(0)
    /// ```
    fn getAttributes(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] name: Option<PhpString>,
        #[php(default = 0)] flags: i64,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let constant = this_name(req, this.object(), "ReflectionConstant")?;
        let attributes = req
            .symbols()
            .constant_attributes(&constant_key(&constant))
            .to_vec();
        let file = this_global_constant(req, this.object())
            .ok()
            .and_then(|name| req.symbols().constant_file(&name).map(<[u8]>::to_vec));
        attr::reflect_attributes(
            req,
            &attributes,
            attr::Subject::new(None, attr::TARGET_CONST, file.as_deref()),
            name,
            flags,
        )
    }
}

/// `enum PropertyHookType: string` as the stub declares it and
/// `zend_register_internal_enum` links it: `BackedEnum` implemented, and
/// `$name` and `$value` as `zend_enum_register_props` declares them.
/// `cases`, `from` and `tryFrom` are the engine's, which gives them to
/// every enum it links as `zend_enum_register_funcs` does. Registered
/// last, after `ReflectionConstant`, as `PHP_MINIT_FUNCTION(reflection)`
/// has it.
#[php_class(backed = string)]
pub(crate) enum PropertyHookType {
    #[php(value = "get")]
    Get,
    #[php(value = "set")]
    Set,
}

#[php_class]
impl PropertyHookType {}

impl PropertyHookType {
    /// The hook a case names.
    fn kind(self) -> HookKind {
        match self {
            PropertyHookType::Get => HookKind::Get,
            PropertyHookType::Set => HookKind::Set,
        }
    }
}
