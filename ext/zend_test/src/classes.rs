//! The test classes, enums, attributes and constants of php's
//! `ext/zend_test/test.c` and `test.stub.php`, registered in `MINIT`'s
//! order. What the attributes cannot say, a type `gen_stub.php` writes
//! by hand, a doc comment it keeps and a trait's kind, is laid onto
//! the declarations by [`ExtensionBuilder::class_reshaped`].

use php_ext::abi::{
    ConstArray, ConstValue, ExtensionBuilder, Instance, ObjectHandlers, Raw, Static, This, Typed,
    Union, Untyped, Variadic,
};
use php_ext::attr::{php_class, php_const};
use php_ext::call::{call, new_object};
use php_ext::decl::{
    Attribute, AttributeTarget, AttributeValidator, ClassDecl, ClassKind, Name, Type,
};
use php_ext::raising::{Thrown, arg_type_error, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::functions::{dnf_type, lorem_ipsum};

/// The classes in `MINIT`'s order, the constants `register_test_symbols`
/// registers between the trait and the attributes.
pub(crate) fn declared(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder
        .class_reshaped::<_ZendTestInterface>(|class| {
            if let Some(dummy) = class.constants.first_mut() {
                dummy.doc_comment = Some(lorem_ipsum(8, "8.2"));
            }
        })
        .class_reshaped::<_ZendTestClass>(|class| {
            let mut nullable = named_union(&[b"stdClass", b"Iterator"]);
            if let Type::Union(types) = &mut nullable {
                types.push(Type::Null);
            }
            retype(class, b"classUnionProp", nullable);

            retype(
                class,
                b"classIntersectionProp",
                intersection(&[b"Traversable", b"Countable"]),
            );
            retype(class, b"dnfProperty", dnf_type());
            for method in &mut class.methods {
                match &*method.function.name.0 {
                    // An instance method whose body reads no receiver,
                    // which `zend_call_method` serves with none.
                    b"returnsStatic" => method.is_instance = true,
                    // The stub spells the class as written, `stdclass`.
                    b"takesUnionType" => {
                        if let Some(arg) = method.function.parameters.first_mut() {
                            arg.ty = Some(named_union(&[b"stdclass", b"Iterator"]));
                        }
                    }
                    _ => {}
                }
            }
            if let Some(last) = class
                .properties
                .iter_mut()
                .find(|p| *p.name.0 == *b"finalProp")
            {
                last.is_final = true;
            }
        })
        .class::<_ZendTestChildClass>()
        .class::<ZendTestGenStubFlagCompatibilityTest>()
        .class::<ZendAttributeTest>()
        .class_reshaped::<_ZendTestTrait>(|class| {
            class.kind = ClassKind::Trait;
            retype(
                class,
                b"classUnionProp",
                named_union(&[b"Traversable", b"Countable"]),
            );
        })
        .constants(constants::declared())
        .constants(ns2_constants::declared())
        .constants(sub_constants::declared())
        .class_reshaped::<ZendTestAttribute>(|class| {
            class.attribute_validator = Some(AttributeValidator(only_classes));
        })
        .class::<ZendTestRepeatableAttribute>()
        .class::<ZendTestParameterAttribute>()
        .class_reshaped::<ZendTestPropertyAttribute>(|class| {
            class.doc_comment = Some(lorem_ipsum(4, "8.1"));
            if let Some(parameter) = class.properties.first_mut() {
                parameter.doc_comment = Some(lorem_ipsum(8, "8.4"));
            }
        })
        .class::<ZendTestAttributeWithArguments>()
        .class::<ZendTestClassWithMethodWithParameterAttribute>()
        .class::<ZendTestChildClassWithMethodWithParameterAttribute>()
        .class::<ZendTestClassWithPropertyAttribute>()
        .class::<ZendTestForbidDynamicCall>()
        .class::<ns::Foo>()
        .class::<ns::UnlikelyCompileError>()
        .class::<ns::NotUnlikelyCompileError>()
        .class::<ns::Bar>()
        .class_reshaped::<ns2::Foo>(|class| {
            let (sub, bar): (&[u8], &[u8]) = (b"ZendTestNS2\\ZendSubNS\\Foo", b"ZendTestNS\\Bar");
            retype(class, b"intersectionProp", intersection(&[sub, bar]));
            retype(class, b"unionProp", named_union(&[sub, bar]));
        })
        .class::<ns2::sub::Foo>()
        .class::<ZendTestUnitEnum>()
        .class::<ZendTestStringEnum>()
        .class::<ZendTestIntEnum>()
        .class::<ZendTestEnumWithInterface>()
        .class::<_ZendTestMagicCall>()
        .class::<_ZendTestMagicCallForward>()
}

/// A property's type laid on by hand, as `MINIT` declares it.
fn retype(class: &mut ClassDecl, property: &[u8], ty: Type) {
    if let Some(declared) = class.properties.iter_mut().find(|p| *p.name.0 == *property) {
        declared.ty = Some(ty);
    }
}

fn named_union(names: &[&[u8]]) -> Type {
    Type::Union(
        names
            .iter()
            .map(|name| Type::Named(Name((*name).into())))
            .collect(),
    )
}

fn intersection(names: &[&[u8]]) -> Type {
    Type::Intersection(
        names
            .iter()
            .map(|name| Type::Named(Name((*name).into())))
            .collect(),
    )
}

/// Core's classes the declarations name.
#[php_class(extern)]
#[allow(non_camel_case_types)]
pub(crate) struct stdClass;
#[php_class(extern)]
pub(crate) struct Iterator;
#[php_class(extern)]
pub(crate) struct Throwable;
#[php_class(extern)]
pub(crate) struct Exception;

/// The constants of `test.stub.php`'s namespaces, in its order.
///
/// ```php
/// var_dump(ZEND_CONSTANT_A, ZendTestNS2\ZEND_CONSTANT_A, ZendTestNS2\ZendSubNS\ZEND_CONSTANT_A);
/// var_dump(ZEND_TEST_DEPRECATED, ZEND_TEST_ATTRIBUTED_CONSTANT);
/// ```
/// ```expect
/// string(6) "global"
/// string(10) "namespaced"
/// string(10) "namespaced"
///
/// Deprecated: Constant ZEND_TEST_DEPRECATED is deprecated in %s on line 3
///
/// Deprecated: Constant ZEND_TEST_ATTRIBUTED_CONSTANT is deprecated since version 1.5, use something else in %s on line 3
/// int(42)
/// int(42)
/// ```
#[php_const]
mod constants {
    #[php(deprecated)]
    pub(crate) const ZEND_TEST_DEPRECATED: i64 = 42;
    pub(crate) const ZEND_CONSTANT_A: &str = "global";
    #[php(deprecated(message = "use something else", since = "version 1.5"))]
    pub(crate) const ZEND_TEST_ATTRIBUTED_CONSTANT: i64 = 42;
}

/// `ZendTestNS2\ZEND_CONSTANT_A`.
#[php_const(namespace(ZendTestNS2))]
mod ns2_constants {
    pub(crate) const ZEND_CONSTANT_A: &str = "namespaced";
}

/// `ZendTestNS2\ZendSubNS\ZEND_CONSTANT_A`, which the stub writes as
/// `\ZendTestNS2\ZEND_CONSTANT_A`.
#[php_const(namespace(ZendTestNS2::ZendSubNS))]
mod sub_constants {
    pub(crate) const ZEND_CONSTANT_A: &str = "namespaced";
}

/// `interface _ZendTestInterface`, its one constant with the doc comment
/// its stub exposes.
///
/// ```php
/// var_dump(_ZendTestInterface::DUMMY);
/// echo (new ReflectionClassConstant('_ZendTestInterface', 'DUMMY'))->getDocComment(), "\n";
/// ```
/// ```expect
/// int(0)
/// /**
///          * "Lorem ipsum"
///          * @see https://www.php.net
///          * @since 8.2
///          */
/// ```
#[php_class(interface)]
pub(crate) struct _ZendTestInterface;

#[php_class]
impl _ZendTestInterface {
    const DUMMY: ConstValue = ConstValue::Int(0);
}

/// `class _ZendTestClass implements _ZendTestInterface`, aliased
/// `_ZendTestClassAlias`: typed constants and properties of each shape,
/// and a `get_method` that answers `test` with a trampoline
/// ([`TestClassHandlers`]).
///
/// ```php
/// $o = new _ZendTestClass;
/// var_dump($o instanceof _ZendTestClassAlias, _ZendTestClass::is_object(), (string) $o, $o->test(), $o->TeSt(1), _ZendTestClass::Test());
/// var_dump(_ZendTestClass::TYPED_CLASS_CONST2, $o->intProp, _ZendTestClass::$staticIntProp);
/// var_dump(get_class($o->returnsStatic()), get_class(_ZendTestClass::variadicTest("a", new ArrayIterator([]))));
/// try { $o->returnsThrowable(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { _ZendTestClass::variadicTest(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $o->takesUnionType(new ArrayObject); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($o->takesUnionType(new stdClass));
/// ```
/// ```expect
/// Deprecated: Method _ZendTestClass::__toString() is deprecated in %s on line 3
/// bool(true)
/// int(42)
/// string(0) ""
/// string(4) "test"
/// string(4) "TeSt"
/// string(4) "Test"
/// int(42)
/// int(123)
/// int(123)
/// string(14) "_ZendTestClass"
/// string(14) "_ZendTestClass"
/// Dummy
/// _ZendTestClass::variadicTest(): Argument #1 must be of class Iterator or a string, int given
/// _ZendTestClass::takesUnionType(): Argument #1 ($arg) must be of type stdclass|Iterator, ArrayObject given
/// NULL
/// ```
#[php_class(
    alias = "_ZendTestClassAlias",
    implements(_ZendTestInterface),
    handlers = crate::classes::TEST_CLASS
)]
pub(crate) struct _ZendTestClass;

#[php_class]
impl _ZendTestClass {
    const TYPED_CLASS_CONST1: Typed<Value, ConstArray> = Typed::new(ConstArray(&[]));
    const TYPED_CLASS_CONST2: Typed<Union<i64, PhpArray>, i64> = Typed::new(42);
    const TYPED_CLASS_CONST3: Typed<Union<i64, PhpString>, i64> = Typed::new(1);
    #[php(deprecated)]
    const ZEND_TEST_DEPRECATED: i64 = 42;
    #[php(deprecated(message = "custom message"))]
    const ZEND_TEST_DEPRECATED_ATTR: i64 = 42;
    #[php(property, static)]
    const _StaticProp: Untyped<Value> = ConstValue::Null;
    #[php(property, static)]
    const staticIntProp: i64 = 123;
    #[php(property)]
    const intProp: i64 = 123;
    #[php(property)]
    const classProp: Option<Instance<stdClass>> = None;
    #[php(property)]
    const classUnionProp: Value = ConstValue::Null;
    #[php(property)]
    const classIntersectionProp: Value;
    #[php(property, readonly)]
    const readonlyProp: i64;
    #[php(property)]
    const finalProp: i64;
    #[php(property)]
    const dnfProperty: Value;

    /// `static is_object(): int`, 42, where Core's `is_object()` answers
    /// a bool.
    fn is_object() -> i64 {
        42
    }

    /// `__toString(): string`, `@deprecated`, empty.
    #[php(deprecated)]
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let _ = this;
        PhpString::new(req, b"")
    }

    /// `returnsStatic(): static`, a new object of the called class: an
    /// instance method, laid on by [`declared`], whose body reads no
    /// receiver.
    fn returnsStatic(req: &mut Request) -> Result<Owner<Static>, Thrown> {
        called(req)
    }

    /// `returnsThrowable(): Throwable`, which throws `Error`, "Dummy".
    fn returnsThrowable(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Instance<Throwable>, Thrown> {
        let _ = this;
        Err(throw_error(req, "Error", "Dummy"))
    }

    /// `static variadicTest(string|Iterator ...$elements): static`: each
    /// element held by the body, then a new object of the called class.
    fn variadicTest(
        req: &mut Request,
        elements: Variadic<Raw<Union<PhpString, Instance<Iterator>>>>,
    ) -> Result<Owner<Static>, Thrown> {
        let iterator = php_ext::call::lookup_class(req, b"Iterator")?;
        for (at, element) in elements.iter().enumerate() {
            let held = match element.value().as_ref() {
                ValueRef::Str(_) => true,
                ValueRef::Object(object) => {
                    iterator.is_some_and(|class| object.instance_of(req, class))
                }
                _ => false,
            };
            if !held {
                let given = php_ext::coerce::type_name(req, &element.value());
                let text = format!("be of class Iterator or a string, {given} given");
                let position = u32::try_from(at + 1).unwrap_or(u32::MAX);
                return Err(arg_type_error(req, position, &text));
            }
        }
        called(req)
    }

    /// `takesUnionType(stdclass|Iterator $arg): void`: an object held
    /// to the declared type by the body.
    fn takesUnionType(
        req: &mut Request,
        this: This<Self>,
        arg: Raw<Union<Instance<stdClass>, Instance<Iterator>>>,
    ) -> Result<(), Thrown> {
        let _ = this;
        let given = arg.value();
        let ValueRef::Object(object) = given.as_ref() else {
            let text = format!(
                "be of type object, {} given",
                php_ext::coerce::value_name(req, &arg.value())
            );
            return Err(arg_type_error(req, 1, &text));
        };
        let mut is = |name: &[u8]| -> Result<bool, Thrown> {
            Ok(php_ext::call::lookup_class(req, name)?
                .is_some_and(|class| object.instance_of(req, class)))
        };
        if is(b"stdClass")? || is(b"Iterator")? {
            return Ok(());
        }
        let text = format!(
            "be of type stdclass|Iterator, {} given",
            req.symbols().class_name(object.class())
        );
        Err(arg_type_error(req, 1, &text))
    }
}

/// `object_init_ex(return_value, zend_get_called_scope(execute_data))`.
fn called(req: &mut Request) -> Result<Owner<Static>, Thrown> {
    let Some(class) = req.frames().called_scope() else {
        return Err(throw_error(
            req,
            "Error",
            "Cannot use \"static\" when no class scope is active",
        ));
    };
    Ok(new_object(req, class)?.into_static())
}

/// `zend_test_class_handlers`: `std_object_handlers` with a `get_method`
/// that answers `test`, in any case, with a trampoline named as the
/// call wrote it; and the class's `get_static_method`, which answers it
/// on the class with a static one.
pub(crate) struct TestClassHandlers;

/// The one table.
pub(crate) static TEST_CLASS: TestClassHandlers = TestClassHandlers;

impl ObjectHandlers for TestClassHandlers {
    fn get_method_as(
        &self,
        req: &mut Request,
        object: PhpObject,
        lowered: &[u8],
        written: &[u8],
    ) -> Result<Option<php_ext::call::Function>, Thrown> {
        if lowered == b"test" {
            return Ok(req.symbols_mut().native_trampoline(written, zend_test_func));
        }
        self.get_method(req, object, lowered)
    }

    /// `zend_test_class_static_method_get`.
    fn get_static_method(
        &self,
        req: &mut Request,
        _class: php_ext::values::ClassId,
        lowered: &[u8],
        written: &[u8],
    ) -> Result<Option<php_ext::call::Function>, Thrown> {
        if lowered == b"test" {
            return Ok(req
                .symbols_mut()
                .native_static_trampoline(written, zend_test_func));
        }
        Ok(None)
    }
}

/// `ZEND_FUNCTION(zend_test_func)`: the name the trampoline was made
/// under.
fn zend_test_func(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    let name = req.frames().active_function_name();
    Ok(PhpString::new(req, name.as_bytes())?.into_value())
}

/// `class _ZendTestChildClass extends _ZendTestClass`, whose
/// `returnsThrowable` narrows the return to `Exception`.
///
/// ```php
/// try { (new _ZendTestChildClass)->returnsThrowable(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// echo (new ReflectionMethod('_ZendTestChildClass', 'returnsThrowable'))->getReturnType(), "\n";
/// ```
/// ```expect
/// Dummy
/// Exception
/// ```
#[php_class(extends(_ZendTestClass))]
pub(crate) struct _ZendTestChildClass;

#[php_class]
impl _ZendTestChildClass {
    /// `returnsThrowable(): Exception`.
    fn returnsThrowable(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Instance<Exception>, Thrown> {
        let _ = this;
        Err(throw_error(req, "Error", "Dummy"))
    }
}

/// `final class ZendTestGenStubFlagCompatibilityTest`,
/// `@not-serializable`.
///
/// ```php
/// try { serialize(new ZendTestGenStubFlagCompatibilityTest); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Serialization of 'ZendTestGenStubFlagCompatibilityTest' is not allowed
/// ```
#[php_class(final, not_serializable)]
pub(crate) struct ZendTestGenStubFlagCompatibilityTest;

#[php_class]
impl ZendTestGenStubFlagCompatibilityTest {}

/// `class ZendAttributeTest`: a constant, a property and a method, each
/// with attributes.
///
/// ```php
/// $r = new ReflectionClassConstant('ZendAttributeTest', 'TEST_CONST');
/// var_dump(count($r->getAttributes()), (new ZendAttributeTest)->testMethod());
/// ```
/// ```expect
/// int(2)
/// bool(true)
/// ```
#[php_class]
pub(crate) struct ZendAttributeTest;

#[php_class]
impl ZendAttributeTest {
    #[php(
        attribute(ZendTestRepeatableAttribute),
        attribute(ZendTestRepeatableAttribute)
    )]
    const TEST_CONST: ConstValue = ConstValue::Int(1);
    #[php(
        property,
        attribute(ZendTestRepeatableAttribute),
        attribute(ZendTestPropertyAttribute("testProp"))
    )]
    const testProp: Untyped<Value> = ConstValue::Null;

    /// `#[ZendTestAttribute] testMethod(): bool`, true.
    #[php(attribute(ZendTestAttribute))]
    fn testMethod(this: This<Self>) -> bool {
        let _ = this;
        true
    }
}

/// `trait _ZendTestTrait`, its kind laid on by [`declared`].
///
/// ```php
/// class UsesTrait { use _ZendTestTrait; }
/// var_dump((new UsesTrait)->testMethod(), property_exists('UsesTrait', 'classUnionProp'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
///
/// A class that takes the method in under an alias has a copy of its
/// own, `zend_add_trait_method`'s, named by the alias and under the
/// alias's visibility, served by the trait's handler:
///
/// ```php
/// class Foo { use _ZendTestTrait { testMethod as aliased; testMethod as protected prot; } }
/// $f = new Foo();
/// var_dump($f->aliased());
/// try { $f->prot(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $f->aliased(1,2); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// Call to protected method Foo::prot() from global scope
/// Foo::aliased() expects exactly 0 arguments, 2 given
/// ```
///
/// The copy keeps the trait's extension, `internal_function.module`, which
/// reflection names though its scope is a user class:
///
/// ```php
/// class Bar { use _ZendTestTrait; }
/// echo new ReflectionMethod('Bar', 'testMethod');
/// var_dump((new ReflectionMethod('Bar', 'testMethod'))->getExtensionName());
/// ```
/// ```expect
/// Method [ <internal:zend_test> public method testMethod ] {
///
///   - Parameters [0] {
///   }
///   - Return [ bool ]
/// }
/// string(9) "zend_test"
/// ```
#[php_class]
pub(crate) struct _ZendTestTrait;

#[php_class]
impl _ZendTestTrait {
    #[php(property)]
    const testProp: Untyped<Value> = ConstValue::Null;
    #[php(property)]
    const classUnionProp: Value;

    /// `testMethod(): bool`, true.
    fn testMethod(this: This<Self>) -> bool {
        let _ = this;
        true
    }
}

/// `#[Attribute(Attribute::TARGET_ALL)] final class ZendTestAttribute`.
///
/// ```php
/// #[ZendTestAttribute] class Marked {}
/// var_dump((new ReflectionClass('Marked'))->getAttributes()[0]->newInstance());
/// ```
/// ```expect
/// object(ZendTestAttribute)#1 (0) {
/// }
/// ```
#[php_class(final, attribute_class(Attribute::TARGET_ALL))]
pub(crate) struct ZendTestAttribute;

#[php_class]
impl ZendTestAttribute {}

/// `zend_attribute_validate_zendtestattribute`, the validator `MINIT`
/// hangs on `ZendTestAttribute`: every target its `TARGET_ALL` allows
/// but a class refused, at compile time where a program applies it.
///
/// ```php
/// #[ZendTestAttribute]
/// function foo() {
/// }
/// ```
/// ```expect exit=255
/// Fatal error: Only classes can be marked with #[ZendTestAttribute] in %s on line 3
/// ```
///
/// `#[\DelayedTargetValidation]` defers the refusal to
/// `ReflectionAttribute::newInstance`, and a native's own use of the
/// attribute, which no compiler saw, is never refused:
///
/// ```php
/// #[DelayedTargetValidation]
/// #[ZendTestAttribute]
/// function f() {}
/// class C { #[DelayedTargetValidation] #[ZendTestAttribute] const K = 1; }
/// foreach ([new ReflectionFunction('f'), new ReflectionClassConstant('C', 'K')] as $r) {
///     try { $r->getAttributes()[1]->newInstance(); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// }
/// #[ZendTestAttribute] class D {}
/// var_dump(get_class((new ReflectionClass('D'))->getAttributes()[0]->newInstance()));
/// var_dump(get_class((new ReflectionMethod('ZendAttributeTest', 'testMethod'))->getAttributes()[0]->newInstance()));
/// ```
/// ```expect
/// Error: Only classes can be marked with #[ZendTestAttribute]
/// Error: Only classes can be marked with #[ZendTestAttribute]
/// string(17) "ZendTestAttribute"
/// string(17) "ZendTestAttribute"
/// ```
fn only_classes(_: &Attribute, target: AttributeTarget, _: Option<&[u8]>) -> Option<String> {
    (target != AttributeTarget::Class)
        .then(|| "Only classes can be marked with #[ZendTestAttribute]".to_owned())
}

/// `#[Attribute(Attribute::TARGET_ALL|Attribute::IS_REPEATABLE)] final
/// class ZendTestRepeatableAttribute`.
///
/// ```php
/// #[ZendTestRepeatableAttribute, ZendTestRepeatableAttribute] class Twice {}
/// var_dump(count((new ReflectionClass('Twice'))->getAttributes()));
/// #[ZendTestRepeatableAttribute] #[ZendTestRepeatableAttribute] const MY_CONST = true;
/// ```
/// ```expect
/// int(2)
/// ```
#[php_class(
    final,
    attribute_class(Attribute::TARGET_ALL | Attribute::IS_REPEATABLE)
)]
pub(crate) struct ZendTestRepeatableAttribute;

#[php_class]
impl ZendTestRepeatableAttribute {}

/// `#[Attribute(Attribute::TARGET_PARAMETER)] final class
/// ZendTestParameterAttribute`.
///
/// ```php
/// var_dump(new ZendTestParameterAttribute("p"));
/// ```
/// ```expect
/// object(ZendTestParameterAttribute)#1 (1) {
///   ["parameter"]=>
///   string(1) "p"
/// }
/// ```
///
/// The compiler reads its targets as it reads Core's attributes': on a
/// promoted parameter it stays on the parameter alone, and on a function
/// or twice over it is php's compile-time error.
///
/// ```php
/// class AttrTest { function __construct(#[ZendTestParameterAttribute('foo')] public $param) {} }
/// $ref = new ReflectionClass(AttrTest::class);
/// var_dump(count($ref->getConstructor()->getParameters()[0]->getAttributes()));
/// var_dump(count($ref->getProperty('param')->getAttributes()));
/// ```
/// ```expect
/// int(1)
/// int(0)
/// ```
///
/// ```php
/// #[ZendTestParameterAttribute('f')]
/// function f() {}
/// ```
/// ```expect exit=255
/// Fatal error: Attribute "ZendTestParameterAttribute" cannot target function (allowed targets: parameter) in %s on line 3
/// ```
///
/// ```php
/// function f(#[ZendTestParameterAttribute('a'), ZendTestParameterAttribute('b')] $a) {}
/// ```
/// ```expect exit=255
/// Fatal error: Attribute "ZendTestParameterAttribute" must not be repeated in %s on line 2
/// ```
#[php_class(final, attribute_class(Attribute::TARGET_PARAMETER))]
pub(crate) struct ZendTestParameterAttribute;

#[php_class]
impl ZendTestParameterAttribute {
    #[php(property)]
    const parameter: PhpString;

    /// `__construct(string $parameter)`, into the one property.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        parameter: PhpString,
    ) -> Result<(), Thrown> {
        let parameter = Owner::take(req, Value::string(parameter));
        this.object().set_slot(req, 0, parameter)
    }
}

/// `#[Attribute(Attribute::TARGET_PROPERTY)] final class
/// ZendTestPropertyAttribute`, the doc comments its stub exposes laid on
/// by [`declared`].
///
/// On a promoted parameter it stays on the property alone:
///
/// ```php
/// class AttrTest { function __construct(#[ZendTestPropertyAttribute('foo')] public $param) {} }
/// $ref = new ReflectionClass(AttrTest::class);
/// var_dump(count($ref->getConstructor()->getParameters()[0]->getAttributes()));
/// var_dump(count($ref->getProperty('param')->getAttributes()));
/// ```
/// ```expect
/// int(0)
/// int(1)
/// ```
///
/// ```php
/// var_dump(new ZendTestPropertyAttribute("p"));
/// echo (new ReflectionClass('ZendTestPropertyAttribute'))->getDocComment(), "\n";
/// ```
/// ```expect
/// object(ZendTestPropertyAttribute)#1 (1) {
///   ["parameter"]=>
///   string(1) "p"
/// }
/// /**
///      * "Lorem ipsum"
///      * @see https://www.php.net
///      * @since 8.1
///      */
/// ```
#[php_class(final, attribute_class(Attribute::TARGET_PROPERTY))]
pub(crate) struct ZendTestPropertyAttribute;

#[php_class]
impl ZendTestPropertyAttribute {
    #[php(property)]
    const parameter: PhpString;

    /// `__construct(string $parameter)`, into the one property.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        parameter: PhpString,
    ) -> Result<(), Thrown> {
        let parameter = Owner::take(req, Value::string(parameter));
        this.object().set_slot(req, 0, parameter)
    }
}

/// `ZendTestAttributeWithArguments`, a final attribute class of every
/// target, its one readonly `mixed` property set by its constructor.
///
/// ```php
/// var_dump(new ZendTestAttributeWithArguments([1]));
/// ```
/// ```expect
/// object(ZendTestAttributeWithArguments)#1 (1) {
///   ["arg"]=>
///   array(1) {
///     [0]=>
///     int(1)
///   }
/// }
/// ```
#[php_class(final, attribute_class(Attribute::TARGET_ALL))]
pub(crate) struct ZendTestAttributeWithArguments;

#[php_class]
impl ZendTestAttributeWithArguments {
    #[php(property, readonly)]
    const arg: Value;

    /// `__construct(mixed $arg)`, `zend_update_property_ex` into `$arg`
    /// from the class's own scope.
    fn __construct(req: &mut Request, this: This<Self>, arg: Value) -> Result<(), Thrown> {
        let arg = Owner::take(req, arg);
        php_ext::call::write_prop(req, &this.object(), b"arg", arg)
    }
}

/// `class ZendTestClassWithMethodWithParameterAttribute`, each method's
/// parameter with an attribute.
///
/// ```php
/// $o = new ZendTestClassWithMethodWithParameterAttribute;
/// var_dump($o->no_override("a"), $o->override("a"));
/// ```
/// ```expect
/// int(2)
/// int(3)
/// ```
#[php_class]
pub(crate) struct ZendTestClassWithMethodWithParameterAttribute;

#[php_class]
impl ZendTestClassWithMethodWithParameterAttribute {
    /// `final no_override(#[ZendTestParameterAttribute("value2")] string
    /// $parameter): int`, 2.
    #[php(final)]
    fn no_override(
        this: This<Self>,
        #[php(attribute(ZendTestParameterAttribute("value2")))] parameter: PhpString,
    ) -> i64 {
        let _ = (this, parameter);
        2
    }

    /// `override(#[ZendTestParameterAttribute("value3")] string
    /// $parameter): int`, 3.
    fn r#override(
        this: This<Self>,
        #[php(attribute(ZendTestParameterAttribute("value3")))] parameter: PhpString,
    ) -> i64 {
        let _ = (this, parameter);
        3
    }
}

/// `class ZendTestChildClassWithMethodWithParameterAttribute extends
/// ZendTestClassWithMethodWithParameterAttribute`.
///
/// ```php
/// var_dump((new ZendTestChildClassWithMethodWithParameterAttribute)->override("a"));
/// ```
/// ```expect
/// int(4)
/// ```
#[php_class(extends(ZendTestClassWithMethodWithParameterAttribute))]
pub(crate) struct ZendTestChildClassWithMethodWithParameterAttribute;

#[php_class]
impl ZendTestChildClassWithMethodWithParameterAttribute {
    /// `override(#[ZendTestParameterAttribute("value4")] string
    /// $parameter): int`, 4.
    fn r#override(
        this: This<Self>,
        #[php(attribute(ZendTestParameterAttribute("value4")))] parameter: PhpString,
    ) -> i64 {
        let _ = (this, parameter);
        4
    }
}

/// `class ZendTestClassWithPropertyAttribute`, `string $attributed` with
/// the `#[ZendTestAttribute]` `MINIT` adds to it.
///
/// ```php
/// var_dump((new ReflectionProperty('ZendTestClassWithPropertyAttribute', 'attributed'))->getAttributes()[0]->getName());
/// ```
/// ```expect
/// string(17) "ZendTestAttribute"
/// ```
#[php_class]
pub(crate) struct ZendTestClassWithPropertyAttribute;

#[php_class]
impl ZendTestClassWithPropertyAttribute {
    #[php(property, attribute(ZendTestAttribute))]
    const attributed: PhpString;
}

/// `final class ZendTestForbidDynamicCall`: each method refuses a
/// dynamic call, `zend_forbid_dynamic_call`.
///
/// ```php
/// $o = new ZendTestForbidDynamicCall;
/// $o->call();
/// ZendTestForbidDynamicCall::callStatic();
/// try { $f = [$o, 'call']; $f(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $f = 'ZendTestForbidDynamicCall::callStatic'; $f(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot call ZendTestForbidDynamicCall::call() dynamically
/// Cannot call ZendTestForbidDynamicCall::callStatic() dynamically
/// ```
#[php_class(final)]
pub(crate) struct ZendTestForbidDynamicCall;

#[php_class]
impl ZendTestForbidDynamicCall {
    /// `call(): void`.
    fn call(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let _ = this;
        req.frames_mut().forbid_dynamic_call()
    }

    /// `static callStatic(): void`.
    fn callStatic(req: &mut Request) -> Result<(), Thrown> {
        req.frames_mut().forbid_dynamic_call()
    }
}

/// `namespace ZendTestNS`.
pub(crate) mod ns {
    use php_ext::abi::This;
    use php_ext::attr::php_class;

    /// `class ZendTestNS\Foo`, a tentative return.
    ///
    /// ```php
    /// var_dump((new ZendTestNS\Foo)->method());
    /// class Sub extends ZendTestNS\Foo { function method() {} }
    /// ```
    /// ```expect
    /// Deprecated: Return type of Sub::method() should either be compatible with ZendTestNS\Foo::method(): int, or the #[\ReturnTypeWillChange] attribute should be used to temporarily suppress the notice in %s on line 3
    /// int(0)
    /// ```
    #[php_class(namespace(ZendTestNS))]
    pub(crate) struct Foo;

    #[php_class]
    impl Foo {
        /// `method(): int`, `@tentative-return-type`, 0.
        #[php(tentative)]
        fn method(this: This<Self>) -> i64 {
            let _ = this;
            0
        }
    }

    /// `interface ZendTestNS\Bar`.
    #[php_class(namespace(ZendTestNS), interface)]
    pub(crate) struct Bar;

    #[php_class]
    impl Bar {}

    /// `class ZendTestNS\UnlikelyCompileError`, whose return names its
    /// own class.
    ///
    /// ```php
    /// var_dump((new ZendTestNS\UnlikelyCompileError)->method());
    /// ```
    /// ```expect
    /// NULL
    /// ```
    #[php_class(namespace(ZendTestNS))]
    pub(crate) struct UnlikelyCompileError;

    #[php_class]
    impl UnlikelyCompileError {
        /// `method(): ?UnlikelyCompileError`, null.
        fn method(this: This<Self>) -> Option<php_ext::abi::Instance<UnlikelyCompileError>> {
            let _ = this;
            None
        }
    }

    /// `class ZendTestNS\NotUnlikelyCompileError`.
    ///
    /// ```php
    /// var_dump((new ZendTestNS\NotUnlikelyCompileError)->method());
    /// ```
    /// ```expect
    /// NULL
    /// ```
    #[php_class(namespace(ZendTestNS))]
    pub(crate) struct NotUnlikelyCompileError;

    #[php_class]
    impl NotUnlikelyCompileError {
        /// `method(): ?NotUnlikelyCompileError`, null.
        fn method(this: This<Self>) -> Option<php_ext::abi::Instance<NotUnlikelyCompileError>> {
            let _ = this;
            None
        }
    }
}

/// `namespace ZendTestNS2` and `ZendTestNS2\ZendSubNS`.
pub(crate) mod ns2 {
    use php_ext::abi::{Instance, This};
    use php_ext::attr::php_class;
    use php_ext::values::Value;

    /// `class ZendTestNS2\Foo`, a property of the class below it, and an
    /// intersection and a union of it with `ZendTestNS\Bar`, laid on by
    /// [`super::declared`].
    ///
    /// ```php
    /// $foo = new ZendTestNS2\Foo;
    /// $foo->foo = new ZendTestNS2\ZendSubNS\Foo;
    /// var_dump($foo, $foo->method(), $foo->foo->method());
    /// ```
    /// ```expect
    /// object(ZendTestNS2\Foo)#1 (1) {
    ///   ["foo"]=>
    ///   object(ZendTestNS2\ZendSubNS\Foo)#2 (0) {
    ///   }
    ///   ["intersectionProp"]=>
    ///   uninitialized(ZendTestNS2\ZendSubNS\Foo&ZendTestNS\Bar)
    ///   ["unionProp"]=>
    ///   uninitialized(ZendTestNS2\ZendSubNS\Foo|ZendTestNS\Bar)
    /// }
    /// NULL
    /// NULL
    /// ```
    #[php_class(namespace(ZendTestNS2))]
    pub(crate) struct Foo;

    #[php_class]
    impl Foo {
        #[php(property)]
        const foo: Instance<sub::Foo>;
        #[php(property)]
        const intersectionProp: Value;
        #[php(property)]
        const unionProp: Value;

        /// `method(): void`.
        fn method(this: This<Self>) {
            let _ = this;
        }
    }

    /// `namespace ZendTestNS2\ZendSubNS`.
    pub(crate) mod sub {
        use php_ext::abi::This;
        use php_ext::attr::php_class;

        /// `class ZendTestNS2\ZendSubNS\Foo`.
        #[php_class(namespace(ZendTestNS2::ZendSubNS))]
        pub(crate) struct Foo;

        #[php_class]
        impl Foo {
            /// `method(): void`.
            fn method(this: This<Self>) {
                let _ = this;
            }
        }
    }
}

/// `enum ZendTestUnitEnum`.
///
/// ```php
/// var_dump(ZendTestUnitEnum::cases());
/// try { $c = ZendTestUnitEnum::Foo; $c->dynamic = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   enum(ZendTestUnitEnum::Foo)
///   [1]=>
///   enum(ZendTestUnitEnum::Bar)
/// }
/// Cannot create dynamic property ZendTestUnitEnum::$dynamic
/// ```
#[php_class]
pub(crate) enum ZendTestUnitEnum {
    Foo,
    Bar,
}

#[php_class]
impl ZendTestUnitEnum {}

/// `enum ZendTestStringEnum: string`.
///
/// ```php
/// var_dump(ZendTestStringEnum::from("Test2"), ZendTestStringEnum::tryFrom("42"), ZendTestStringEnum::Baz->value);
/// ```
/// ```expect
/// enum(ZendTestStringEnum::Bar)
/// enum(ZendTestStringEnum::FortyTwo)
/// string(7) "Test2\a"
/// ```
#[php_class(backed = string)]
pub(crate) enum ZendTestStringEnum {
    #[php(value = "Test1")]
    Foo,
    #[php(value = "Test2")]
    Bar,
    #[php(value = "Test2\\a")]
    Baz,
    #[php(value = "42")]
    FortyTwo,
}

#[php_class]
impl ZendTestStringEnum {}

/// `enum ZendTestIntEnum: int`.
///
/// ```php
/// var_dump(ZendTestIntEnum::from(3), ZendTestIntEnum::tryFrom(2), ZendTestIntEnum::Baz->value);
/// ```
/// ```expect
/// enum(ZendTestIntEnum::Bar)
/// NULL
/// int(-1)
/// ```
#[php_class(backed = int)]
#[repr(i64)]
pub(crate) enum ZendTestIntEnum {
    Foo = 1,
    Bar = 3,
    Baz = -1,
}

#[php_class]
impl ZendTestIntEnum {}

/// `enum ZendTestEnumWithInterface implements _ZendTestInterface`.
///
/// ```php
/// var_dump(ZendTestEnumWithInterface::Foo instanceof _ZendTestInterface, ZendTestEnumWithInterface::DUMMY);
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// ```
#[php_class(implements(_ZendTestInterface))]
pub(crate) enum ZendTestEnumWithInterface {
    Foo,
    Bar,
}

#[php_class]
impl ZendTestEnumWithInterface {}

/// `class _ZendTestMagicCall`, whose `__call` answers its name and its
/// arguments as a pair.
///
/// ```php
/// var_dump((new _ZendTestMagicCall)->test('a', c: 'c'));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(4) "test"
///   [1]=>
///   array(2) {
///     [0]=>
///     string(1) "a"
///     ["c"]=>
///     string(1) "c"
///   }
/// }
/// ```
#[php_class]
pub(crate) struct _ZendTestMagicCall;

#[php_class]
impl _ZendTestMagicCall {
    /// `__call(string $name, array $args): mixed`, `zend_new_pair`.
    fn __call(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        args: PhpArray,
    ) -> Result<Owner<Value>, Thrown> {
        let _ = this;
        let mut pair = PhpArray::new(req)?;
        pair.append(req, Owner::take(req, Value::string(name)))?;
        pair.append(req, Owner::take(req, Value::array(args)))?;
        Ok(pair.into_value())
    }
}

/// `class _ZendTestMagicCallForward`, whose `__call` calls the function
/// of the method's name with no argument.
///
/// ```php
/// function hello() { return "hi"; }
/// var_dump((new _ZendTestMagicCallForward)->hello());
/// ```
/// ```expect
/// string(2) "hi"
/// ```
#[php_class]
pub(crate) struct _ZendTestMagicCallForward;

#[php_class]
impl _ZendTestMagicCallForward {
    /// `__call(string $name, array $args): mixed`, `call_user_function`
    /// of `$name`, what it returned dereferenced.
    fn __call(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        args: PhpArray,
    ) -> Result<Owner<Value>, Thrown> {
        let _ = (this, args);
        let callable = php_ext::coerce::to_callable(req, &Value::string(name))?;
        call(req, &callable, &[])
    }
}
