//! The three enum reflectors: `ReflectionEnum`, whose cases are the
//! constants of the class that `ZEND_CLASS_CONST_IS_CASE` marks, and
//! `ReflectionEnumUnitCase` with `ReflectionEnumBackedCase` under it,
//! each of which names one case and hands back the instance the engine
//! keeps for it (`zend_enum_get_case`, served here through
//! `call::class_constant`). The three methods `zend_enum_register_funcs`
//! gives every enum, `cases`, `from` and `tryFrom`, are the engine's,
//! which links them onto every enum, a native's and a program's alike.

use php_ext::abi::{Instance, Raw, This, Union};
use php_ext::attr::php_class;
use php_ext::call::class_constant;
use php_ext::decl::{ClassKind, EnumBacking, Type};
use php_ext::raising::{Thrown, fatal, throw, throw_error};
use php_ext::request::Request;
use php_ext::values::{ClassId, Owner, PhpArray, PhpObject, PhpString, Value};

use super::signature::type_factory;
use super::{
    class_constant_named, class_name_of, class_named, class_of, new_object, set_name,
    set_string_prop, this_name,
};

/// `ce->enum_backing_type`: the type of a backed enum's `value`, and
/// `None` for a pure one.
fn backing_of(req: &Request, class: ClassId) -> Option<Type> {
    match req.class_decl(class).map(|decl| decl.backing) {
        Some(EnumBacking::Int) => Some(Type::Int),
        Some(EnumBacking::String) => Some(Type::String),
        _ => None,
    }
}

/// `ZEND_ACC_ENUM` on the class a `ReflectionEnum` names, or php's
/// `ReflectionException` for one that is no enum.
fn this_enum(req: &mut Request, this: PhpObject) -> Result<ClassId, Thrown> {
    let name = this_name(req, this, "ReflectionEnum")?;
    let class = class_named(req, &name)?;
    held_to_enum(req, class)?;
    Ok(class)
}

/// `ReflectionEnum::__construct`'s own refusal, after
/// `reflection_class_object_ctor` has done its half.
fn held_to_enum(req: &mut Request, class: ClassId) -> Result<(), Thrown> {
    if req
        .class_decl(class)
        .is_some_and(|decl| decl.kind == ClassKind::Enum)
    {
        return Ok(());
    }
    let name = class_name_of(req, class);
    Err(throw(
        req,
        "ReflectionException",
        &format!(
            "Class \"{}\" is not an enum",
            String::from_utf8_lossy(&name)
        ),
    ))
}

/// The case names of an enum, in declaration order: the class's own
/// constants, which a `ClassDecl` of kind `Enum` carries as its cases.
fn case_names(req: &Request, class: ClassId) -> Vec<Vec<u8>> {
    req.class_decl(class).map_or_else(Vec::new, |decl| {
        decl.constants
            .iter()
            .filter(|constant| constant.is_case)
            .map(|constant| constant.name.0.to_vec())
            .collect()
    })
}

/// php's `reflection_enum_case_factory`: a `ReflectionEnumBackedCase`
/// for a backed enum and a `ReflectionEnumUnitCase` for a pure one,
/// with `$name` and `$class` written as
/// `reflection_class_constant_factory` writes them.
fn case_factory(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
) -> Result<Owner<PhpObject>, Thrown> {
    let reflector = if backing_of(req, class).is_some() {
        class_named(req, b"ReflectionEnumBackedCase")?
    } else {
        class_named(req, b"ReflectionEnumUnitCase")?
    };
    let object = new_object(req, reflector)?;
    let held = class_name_of(req, class);
    set_name(req, *object, name)?;
    set_string_prop(req, *object, b"class", &held)?;
    Ok(object)
}

/// The enum and the case a `ReflectionEnumUnitCase` names, php's
/// `GET_REFLECTION_OBJECT_PTR(ref)` over `intern->ptr`.
fn this_case(req: &mut Request, this: PhpObject) -> Result<(ClassId, Vec<u8>), Thrown> {
    let name = this_name(req, this, "ReflectionEnumUnitCase")?;
    let Some(class) = this
        .prop(req, b"class")
        .and_then(|value| value.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Err(throw_error(
            req,
            "Error",
            "Typed property ReflectionClassConstant::$class must not be accessed before initialization",
        ));
    };
    let class = class_named(req, &class)?;
    held_to_enum(req, class)?;
    Ok((class, name))
}

/// The one instance the engine keeps for a case, `zend_enum_get_case`,
/// one more owner of it, a value that is that object.
fn case_object(req: &mut Request, class: ClassId, name: &[u8]) -> Result<Owner<Value>, Thrown> {
    let case = class_constant(req, class, name)?;
    if case.as_object().is_some() {
        return Ok(case);
    }
    case.give_up(req);
    Err(fatal(req, "InternalError: an enum case that is no object"))
}

/// `ReflectionEnum` as the stub declares it.
#[php_class(extends(crate::class::ReflectionClass))]
pub(crate) struct ReflectionEnum;

#[php_class]
impl ReflectionEnum {
    /// `ReflectionEnum::__construct(object|string $objectOrClass)`:
    /// `reflection_class_object_ctor` and then php's own refusal for a
    /// class that is no enum.
    ///
    /// ```php
    /// var_dump(new ReflectionEnum('PropertyHookType'));
    /// try { new ReflectionEnum('Exception'); } catch (Throwable $t) { echo get_class($t), ': ', $t->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(ReflectionEnum)#1 (1) {
    ///   ["name"]=>
    ///   string(16) "PropertyHookType"
    /// }
    /// ReflectionException: Class "Exception" is not an enum
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        objectOrClass: Union<PhpObject, PhpString>,
    ) -> Result<(), Thrown> {
        let (class, _) = class_of(req, objectOrClass)?;
        held_to_enum(req, class)?;
        let name = class_name_of(req, class);
        let this = this.object();
        set_name(req, this, &name)?;
        Ok(())
    }

    /// `ReflectionEnum::hasCase(string $name): bool`: whether the
    /// constants table holds a case of that name.
    ///
    /// ```php
    /// $e = new ReflectionEnum('PropertyHookType');
    /// var_dump($e->hasCase('Get'), $e->hasCase('Nope'));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    fn hasCase(req: &mut Request, this: This<Self>, name: PhpString) -> Result<bool, Thrown> {
        let name = name.as_bytes().to_vec();
        let class = this_enum(req, this.object())?;
        Ok(case_names(req, class).contains(&name))
    }

    /// `ReflectionEnum::getCase(string $name): ReflectionEnumUnitCase`: the
    /// case reflector for that name, php's `ReflectionException` for a name
    /// the enum has no case under.
    ///
    /// ```php
    /// $e = new ReflectionEnum('PropertyHookType');
    /// var_dump($e->getCase('Get'));
    /// try { $e->getCase('Nope'); } catch (Throwable $t) { echo get_class($t), ': ', $t->getMessage(), "\n"; }
    /// enum F { case A; const C = 1; }
    /// try { (new ReflectionEnum('F'))->getCase('C'); } catch (ReflectionException $t) { echo $t->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(ReflectionEnumBackedCase)#2 (2) {
    ///   ["name"]=>
    ///   string(3) "Get"
    ///   ["class"]=>
    ///   string(16) "PropertyHookType"
    /// }
    /// ReflectionException: Case PropertyHookType::Nope does not exist
    /// F::C is not a case
    /// ```
    fn getCase(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
    ) -> Result<Owner<Instance<ReflectionEnumUnitCase>>, Thrown> {
        let name = name.as_bytes().to_vec();
        let class = this_enum(req, this.object())?;
        if !case_names(req, class).contains(&name) {
            let held = class_name_of(req, class);
            // A constant of that name that is no case is php's second
            // refusal, `ZEND_CLASS_CONST_IS_CASE` unset.
            let format = if class_constant_named(req, class, &name).is_some() {
                "{}::{} is not a case"
            } else {
                "Case {}::{} does not exist"
            };
            let message = format
                .replacen("{}", &String::from_utf8_lossy(&held), 1)
                .replacen("{}", &String::from_utf8_lossy(&name), 1);
            return Err(throw(req, "ReflectionException", &message));
        }
        let object = case_factory(req, class, &name)?;
        Ok(object.into_instance())
    }

    /// `ReflectionEnum::getCases(): array`: one case reflector per case, in
    /// declaration order.
    ///
    /// ```php
    /// var_dump(count((new ReflectionEnum('PropertyHookType'))->getCases()));
    /// ```
    /// ```expect
    /// int(2)
    /// ```
    ///
    /// A program's enum, pure and backed, its own constant no case:
    ///
    /// ```php
    /// enum Suit: int { case Hearts = 1; case Spades = 2; const Wild = self::Spades; }
    /// enum Pure { case A; }
    /// $s = new ReflectionEnum('Suit');
    /// foreach ($s->getCases() as $case) echo get_class($case), ' ', $case->name, '=', $case->getBackingValue(), "\n";
    /// var_dump($s->isBacked(), (string) $s->getBackingType(), $s->hasCase('Wild'));
    /// $p = new ReflectionEnum('Pure');
    /// var_dump($p->isBacked(), $p->getBackingType(), get_class($p->getCases()[0]), $p->getCase('A')->getValue());
    /// ```
    /// ```expect
    /// ReflectionEnumBackedCase Hearts=1
    /// ReflectionEnumBackedCase Spades=2
    /// bool(true)
    /// string(3) "int"
    /// bool(false)
    /// bool(false)
    /// NULL
    /// string(22) "ReflectionEnumUnitCase"
    /// enum(Pure::A)
    /// ```
    fn getCases(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let class = this_enum(req, this.object())?;
        let mut out = PhpArray::new(req)?;
        for name in case_names(req, class) {
            let object = case_factory(req, class, &name)?;
            out.append(req, object.into_value())?;
        }
        Ok(out)
    }

    /// `ReflectionEnum::isBacked(): bool`: `ce->enum_backing_type != IS_UNDEF`.
    ///
    /// ```php
    /// var_dump((new ReflectionEnum('PropertyHookType'))->isBacked());
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    fn isBacked(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this_enum(req, this.object())?;
        Ok(backing_of(req, class).is_some())
    }

    /// `ReflectionEnum::getBackingType(): ?ReflectionNamedType`: the
    /// backing type as a type reflector, and `null` for a pure enum.
    ///
    /// ```php
    /// var_dump((string) (new ReflectionEnum('PropertyHookType'))->getBackingType());
    /// ```
    /// ```expect
    /// string(6) "string"
    /// ```
    fn getBackingType(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<crate::signature::ReflectionNamedType>>>, Thrown> {
        let class = this_enum(req, this.object())?;
        match backing_of(req, class) {
            Some(backing) => Ok(Some(type_factory(req, &backing, false)?.into_instance())),
            None => Ok(None),
        }
    }
}

/// `ReflectionEnumUnitCase` as the stub declares it.
#[php_class(extends(crate::member::ReflectionClassConstant))]
pub(crate) struct ReflectionEnumUnitCase;

#[php_class]
impl ReflectionEnumUnitCase {
    /// `ReflectionEnumUnitCase::__construct(object|string $class, string
    /// $constant)`: `ReflectionClassConstant`'s constructor and then php's
    /// refusals for a class that is no enum and a constant that is no case.
    ///
    /// ```php
    /// var_dump(new ReflectionEnumUnitCase('PropertyHookType', 'Set'));
    /// try { new ReflectionEnumUnitCase('Nope', 'Set'); } catch (Throwable $t) { echo get_class($t), ': ', $t->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(ReflectionEnumUnitCase)#1 (2) {
    ///   ["name"]=>
    ///   string(3) "Set"
    ///   ["class"]=>
    ///   string(16) "PropertyHookType"
    /// }
    /// ReflectionException: Class "Nope" does not exist
    /// ```
    ///
    /// A constant of an enum that is no case, and a pure enum's case under
    /// `ReflectionEnumBackedCase`, are php's two refusals after it:
    ///
    /// ```php
    /// enum E { case A; const C = 1; }
    /// try { new ReflectionEnumUnitCase('E', 'C'); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// try { new ReflectionEnumBackedCase('E', 'A'); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// try { new ReflectionEnumUnitCase('E', 'Q'); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Constant E::C is not a case
    /// Enum case E::A is not a backed case
    /// Constant E::Q does not exist
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        class: Union<PhpObject, PhpString>,
        constant: PhpString,
    ) -> Result<(), Thrown> {
        let (class, _) = class_of(req, class)?;
        let name = constant.as_bytes().to_vec();
        held_to_enum(req, class)?;
        if class_constant_named(req, class, &name).is_none() {
            // `ReflectionClassConstant::__construct`'s refusal, which the
            // case constructor runs first.
            let held = class_name_of(req, class);
            return Err(throw(
                req,
                "ReflectionException",
                &format!(
                    "Constant {}::{} does not exist",
                    String::from_utf8_lossy(&held),
                    String::from_utf8_lossy(&name)
                ),
            ));
        }
        let held = class_name_of(req, class);
        let this = this.object();
        set_name(req, this, &name)?;
        set_string_prop(req, this, b"class", &held)?;
        let shown = format!(
            "{}::{}",
            String::from_utf8_lossy(&held),
            String::from_utf8_lossy(&name)
        );
        if !case_names(req, class).contains(&name) {
            return Err(throw(
                req,
                "ReflectionException",
                &format!("Constant {shown} is not a case"),
            ));
        }
        // `ReflectionEnumBackedCase::__construct` after the unit case's.
        let backed_reflector =
            class_name_of(req, this.class()).eq_ignore_ascii_case(b"ReflectionEnumBackedCase");
        if backed_reflector && backing_of(req, class).is_none() {
            return Err(throw(
                req,
                "ReflectionException",
                &format!("Enum case {shown} is not a backed case"),
            ));
        }
        Ok(())
    }

    /// `ReflectionEnumUnitCase::getEnum(): ReflectionEnum`: the enum the
    /// case belongs to.
    ///
    /// ```php
    /// $c = (new ReflectionEnum('PropertyHookType'))->getCase('Get');
    /// var_dump($c->getEnum());
    /// ```
    /// ```expect
    /// object(ReflectionEnum)#1 (1) {
    ///   ["name"]=>
    ///   string(16) "PropertyHookType"
    /// }
    /// ```
    fn getEnum(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<ReflectionEnum>>, Thrown> {
        let (class, _) = this_case(req, this.object())?;
        let reflector = class_named(req, b"ReflectionEnum")?;
        let object = new_object(req, reflector)?;
        let name = class_name_of(req, class);
        set_name(req, *object, &name)?;
        Ok(object.into_instance())
    }

    /// `ReflectionEnumUnitCase::getValue(): UnitEnum`: the one instance the
    /// engine keeps for the case, `zend_enum_get_case`.
    ///
    /// ```php
    /// $c = (new ReflectionEnum('PropertyHookType'))->getCase('Get');
    /// var_dump($c->getValue(), $c->getValue() === PropertyHookType::Get);
    /// ```
    /// ```expect
    /// enum(PropertyHookType::Get)
    /// bool(true)
    /// ```
    fn getValue(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Raw<Instance<crate::UnitEnum>>>, Thrown> {
        let (class, name) = this_case(req, this.object())?;
        Ok(case_object(req, class, &name)?.into_raw())
    }
}

/// `ReflectionEnumBackedCase` as the stub declares it.
#[php_class(extends(ReflectionEnumUnitCase))]
pub(crate) struct ReflectionEnumBackedCase;

#[php_class]
impl ReflectionEnumBackedCase {
    #[php(implementation = ReflectionEnumUnitCase::__construct)]
    fn __construct(this: This<Self>, class: Union<PhpObject, PhpString>, constant: PhpString);

    /// `ReflectionEnumBackedCase::getBackingValue(): int|string`: the
    /// case's `value`, `zend_enum_fetch_case_value`.
    ///
    /// ```php
    /// $c = (new ReflectionEnum('PropertyHookType'))->getCase('Set');
    /// var_dump($c->getBackingValue());
    /// ```
    /// ```expect
    /// string(3) "set"
    /// ```
    fn getBackingValue(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Union<i64, PhpString>, Thrown> {
        let (class, name) = this_case(req, this.object())?;
        let case = case_object(req, class, &name)?;
        let value = case
            .as_object()
            .and_then(|object| object.prop(req, b"value"))
            .unwrap_or_default();
        // A view of the string the case's own `value` keeps: the class's
        // constant keeps the case.
        let backing = match (value.as_int(), value.as_php_string()) {
            (Some(int), _) => Some(Union::A(int)),
            (None, Some(string)) => Some(Union::B(string)),
            (None, None) => None,
        };
        case.give_up(req);
        backing.ok_or_else(|| {
            fatal(
                req,
                "InternalError: a backed enum case whose value is neither int nor string",
            )
        })
    }
}
