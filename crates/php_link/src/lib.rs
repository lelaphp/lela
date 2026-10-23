//! What class linking refuses, after `zend_inheritance.c`: the checks
//! `zend_do_inheritance_ex` makes of a class against its parent and
//! `do_interface_implementation` against each interface, in php's order
//! and in php's words, over the declarations the linker binds (design
//! §6.2). A class that links has its record ([`LinkedClass`]): the
//! tables php leaves in the class entry, built once, which a child's link
//! reads off its parent's record rather than walking the chain. A
//! member the record does not carry yet is read off the chain in the
//! order php's tables hold it: a class's own members first, then what
//! it inherited.
//!
//! One function answers for both of php's binds: the early bind
//! while the file compiles, where `zend_try_early_bind` runs these
//! checks and an unresolved one leaves the class to its
//! `ZEND_DECLARE_CLASS`, and that opline as the program serves. So it
//! is a crate of its own, between the unit format it reads and the
//! compiler and the engine that call it (ADR-0012), and reads the
//! classes it links against through a lookup by name each caller hands
//! it. Each refusal is `E_COMPILE_ERROR` at the site php
//! names, the child method's first line for what is about a method and
//! the class's line otherwise.
//!
//! Method compatibility is `zend_do_perform_implementation_check` over
//! the two declarations, with `zend_perform_covariant_type_check` over
//! each type, and a class a type names that no table holds is php's
//! unresolved outcome, which the bind reports as php reports an
//! obligation it could not discharge.
//!
//! A class that uses traits is the one case of copying something:
//! `zend_do_bind_traits` puts the traits' methods, constants and
//! properties into the class's own tables before the parent is
//! inherited, so [`traits`] composes a declaration of the class with
//! them as its own, each method with the [`Body`] it serves, and the
//! checks above are made over that ([`Composed`]).
//!
//! `link.lock` is this crate's public surface.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

use php_unit::Level;
use php_unit::decl::{
    ClassConstDecl, ClassDecl, ClassKind, ConstExpr, EnumBacking, FunctionDecl, HookKind,
    MethodDecl, Name, Parameter, PropertyDecl, Type, Visibility,
};
use php_unit::type_to_string;
use std::fmt;

mod hooks;
mod rows;
mod traits;

pub use rows::{
    Accessor, Accessors, Case, ClassHooks, Exact, Folded, Indexed, LinkedClass, NameHash,
    NameHasher, NameMap, Named, Names, Row, Rows,
};

pub use hooks::{
    HookVariance, Resolved, ResolvedHook, Variance, resolve_property, set_hook_variance,
    verify_hooked_property,
};

const MAY_BE_OBJECT: u32 = 1 << 8;
const MAY_BE_CALLABLE: u32 = 1 << 12;
const MAY_BE_VOID: u32 = 1 << 14;
const MAY_BE_STATIC: u32 = 1 << 15;
const MAY_BE_NEVER: u32 = 1 << 17;

/// Where php raises what a link refused or deprecated: the file and the
/// first line of the declaration it is about, the child method's for a
/// method and the class's for the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub file: Name,
    pub line: u32,
}

/// What `zend_inheritance.c` refuses, one variant per sentence php
/// raises, each holding the names the sentence prints as php prints
/// them; `Display` is the sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// `Interface %s cannot extend class %s`.
    InterfaceExtendsClass { interface: String, class: String },
    /// `Class %s cannot extend enum %s`.
    ExtendsEnum { class: String, parent: String },
    /// `Class %s cannot extend final class %s`.
    ExtendsFinal { class: String, parent: String },
    /// `Class %s cannot extend interface %s` and its trait twin.
    ExtendsNotAClass {
        class: String,
        kind: ClassKind,
        parent: String,
    },
    /// `%s class %s cannot extend %s class %s`, the readonly bits differing.
    ReadonlyClass {
        class: String,
        readonly: bool,
        parent: String,
    },
    /// `Cannot override final property %s::$%s`.
    FinalProperty { parent: String, property: String },
    /// `Cannot redeclare %s%s::$%s as %s%s::$%s`, a class's property on one
    /// side and an instance's on the other.
    PropertyReceiver {
        parent: String,
        class: String,
        property: String,
        parent_static: bool,
    },
    /// `Cannot redeclare %s property %s::$%s as %s %s::$%s`.
    ReadonlyProperty {
        parent: String,
        class: String,
        property: String,
        parent_readonly: bool,
    },
    /// `Set access level of %s::$%s must be %s (as in class %s)%s`.
    SetVisibility {
        class: String,
        property: String,
        parent: String,
        parent_set: Option<Visibility>,
    },
    /// `Access level to %s::$%s must be %s (as in class %s)%s`.
    PropertyVisibility {
        class: String,
        property: String,
        parent: String,
        visibility: Visibility,
    },
    /// `Type of %s::$%s must be %s%s (as in class %s)`, the parent's
    /// type after `subtype of ` or `supertype of ` where
    /// `prop_get_variance` reads it one way only.
    PropertyType {
        class: String,
        property: String,
        ty: String,
        parent: String,
        variance: Variance,
    },
    /// `Type of %s::$%s must be omitted to match the parent definition in class %s`.
    PropertyTypeOmitted {
        class: String,
        property: String,
        parent: String,
    },
    /// `Set type of %s::$%s must be supertype of %s (as in %s %s)`: a
    /// child with no `set` hook of its own over an abstract parent's.
    SetHookType {
        class: String,
        property: String,
        ty: String,
        parent_kind: ClassKind,
        parent: String,
    },
    /// `Type of parameter $%s of hook %s::$%s::set must be compatible
    /// with property type`.
    SetHookParameter {
        parameter: String,
        class: String,
        property: String,
    },
    /// `Cannot override final property hook %s::%s()`.
    FinalHook { parent: String, hook: String },
    /// `Cannot specify default value for virtual hooked property %s::$%s`.
    VirtualDefault { class: String, property: String },
    /// `Get hook of backed property %s::%s with set hook may not return
    /// by reference`, the property named with no `$`, as php names it.
    BackedGetByRef { class: String, property: String },
    /// `Abstract property %s::$%s must specify at least one abstract hook`.
    AbstractWithoutAbstractHook { class: String, property: String },
    /// `%s virtual property %s::$%s must not specify asymmetric
    /// visibility`, `Write-only` where it has no `get` hook and
    /// `Read-only` where it has no `set` hook.
    VirtualAsymmetric {
        write_only: bool,
        class: String,
        property: String,
    },
    /// `%s::%s cannot override final constant %s::%s`.
    FinalConstant {
        holder: String,
        parent: String,
        constant: String,
    },
    /// `%s %s inherits both %s::%s and %s::%s, which is ambiguous`.
    AmbiguousConstant {
        kind: ClassKind,
        class: String,
        holder: String,
        parent: String,
        constant: String,
    },
    /// `Type of %s::%s must be compatible with %s::%s of type %s`, a
    /// typed constant redeclared with no type or one that is no subtype.
    ConstantType {
        holder: String,
        constant: String,
        parent: String,
        ty: String,
    },
    /// `Access level to %s::%s must be %s (as in %s %s)%s`.
    ConstantVisibility {
        class: String,
        constant: String,
        visibility: Visibility,
        parent_kind: ClassKind,
        parent: String,
    },
    /// `Cannot override final method %s::%s()`.
    FinalMethod { parent: String, method: String },
    /// `Cannot make %s method %s::%s() %s in class %s`.
    MethodReceiver {
        parent: String,
        method: String,
        class: String,
        parent_static: bool,
    },
    /// `Cannot make non abstract method %s::%s() abstract in class %s`.
    AbstractMethod {
        parent: String,
        method: String,
        class: String,
    },
    /// `Access level to %s::%s() must be %s (as in class %s)%s`.
    MethodVisibility {
        class: String,
        method: String,
        visibility: Visibility,
        parent: String,
    },
    /// `Declaration of %s must be compatible with %s`, each side as
    /// `zend_get_function_declaration` writes it.
    Incompatible { child: String, parent: String },
    /// `Could not check compatibility between %s and %s, because class
    /// %s is not available`.
    Unavailable {
        child: String,
        parent: String,
        missing: String,
    },
    /// `%s %s cannot implement interface Throwable, extend Exception or
    /// Error instead`, and for an enum, which can extend neither, the
    /// sentence without its advice (`zend_implement_throwable`).
    Throwable { kind: ClassKind, class: String },
    /// `%s cannot implement %s - it is not an interface`, a class or a
    /// trait named in an interface list.
    NotAnInterface { class: String, interface: String },
    /// `%s %s cannot implement previously implemented interface %s`, one
    /// interface named twice in the declaration's own list.
    PreviouslyImplemented {
        kind: ClassKind,
        class: String,
        interface: String,
    },
    /// `Non-enum class %s cannot implement interface %s`.
    EnumInterface { class: String, interface: String },
    /// `Non-backed enum %s cannot implement interface %s`, a pure enum
    /// naming `BackedEnum` (`zend_implement_backed_enum`).
    NonBackedEnumInterface { class: String, interface: String },
    /// `%s %s must implement interface Traversable as part of either
    /// Iterator or IteratorAggregate`, the object's kind first.
    Traversable { kind: ClassKind, class: String },
    /// `Class %s cannot implement both Iterator and IteratorAggregate at
    /// the same time` (`zend_implement_aggregate`, `zend_implement_iterator`).
    IteratorAndAggregate { class: String },
    /// `%s can't be implemented by user classes`, an extension's
    /// `interface_gets_implemented` over a class that descends from none
    /// of the classes the interface names ([`ClassDecl::implementors`]).
    UserImplementor { interface: String },
    /// `Enum %s cannot include magic method %s`
    /// (`zend_verify_enum_magic_methods`).
    EnumMagic { class: String, method: &'static str },
    /// `Cannot redeclare %s::%s()`, an enum that declares a method
    /// `zend_enum_register_funcs` gives it, named as php's known string
    /// spells it, `tryfrom` lowered.
    EnumRedeclare { class: String, method: String },
    /// `Enum %s cannot implement the Serializable interface`
    /// (`zend_verify_enum_interfaces`).
    EnumSerializable { class: String },
    /// `Class %s contains %d abstract method%s and must therefore be
    /// declared abstract or implement the remaining method%s (%s)`, with
    /// every method left, of which php names three; an enum, which
    /// cannot be abstract, and a class declared abstract, which is left
    /// with a private abstract method no other class may implement,
    /// `must implement` them.
    AbstractLeft {
        kind: ClassKind,
        class: String,
        methods: Vec<String>,
        must_implement: bool,
    },
    /// `Trait method %s::%s has not been applied as %s::%s, because of
    /// collision with %s::%s`: two traits' methods under one name.
    TraitCollision(Box<Collision>),
    /// `Could not find trait %s`, an adaptation naming a class no
    /// table holds.
    TraitNotFound { name: String },
    /// `Class %s is not a trait, Only traits may be used in 'as' and
    /// 'insteadof' statements`.
    NotATrait { class: String },
    /// `Required Trait %s wasn't added to %s`.
    TraitNotUsed { name: String, class: String },
    /// `A precedence rule was defined for %s::%s but this method does
    /// not exist`.
    PrecedenceMissing { name: String, method: String },
    /// `Failed to evaluate a trait precedence (%s). Method of trait %s
    /// was defined to be excluded multiple times`.
    ExcludedTwice { method: String, name: String },
    /// `Inconsistent insteadof definition. The method %s is to be used
    /// from %s, but %s is also on the exclude list`.
    InsteadofItself { method: String, name: String },
    /// `An alias was defined for %s::%s but this method does not exist`.
    AliasMissing { name: String, method: String },
    /// `An alias was defined for method %s(), which exists in both %s
    /// and %s. Use %s::%s or %s::%s to resolve the ambiguity`.
    AliasAmbiguous {
        method: String,
        first: String,
        second: String,
    },
    /// `An alias (%s) was defined for method %s(), but this method does
    /// not exist`.
    AliasNowhere { alias: String, method: String },
    /// `The modifiers of the trait method %s() are changed, but this
    /// method does not exist. Error`.
    ModifiersNowhere { method: String },
    /// `%s and %s define the same constant (%s) in the composition of
    /// %s. However, the definition differs and is considered
    /// incompatible. Class was composed`.
    TraitConstant {
        first: String,
        from: String,
        constant: String,
        class: String,
    },
    /// `Cannot use trait %s, because %s::%s conflicts with enum case
    /// %s::%s`.
    TraitConstantCase {
        from: String,
        constant: String,
        class: String,
    },
    /// `%s and %s define the same property ($%s) in the composition of
    /// %s. However, the definition differs and is considered
    /// incompatible. Class was composed`.
    TraitProperty {
        first: String,
        from: String,
        property: String,
        class: String,
    },
    /// `%s and %s define the same hooked property ($%s) in the
    /// composition of %s. Conflict resolution between hooked properties
    /// is currently not supported. Class was composed`: either side
    /// hooked.
    TraitHookedProperty {
        first: String,
        from: String,
        property: String,
        class: String,
    },
    /// `Readonly class %s cannot use trait with a non-readonly property
    /// %s::$%s`.
    ReadonlyTraitProperty {
        class: String,
        from: String,
        property: String,
    },
    /// `Enum %s cannot include properties`, what `zend_verify_enum`
    /// raises of a property a trait gave the enum.
    EnumProperties { class: String },
    /// `%s::%s() has #[\Override] attribute, but no matching parent
    /// method exists`.
    OverrideMethod { class: String, method: String },
    /// `%s::$%s has #[\Override] attribute, but no matching parent
    /// property exists`.
    OverrideProperty { class: String, property: String },
}

/// The six names of a [`Refusal::TraitCollision`]: the trait a method
/// was copied from and its name there, the class and the name it was
/// to go in under, and the trait whose method already stands there and
/// that method's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub from: String,
    pub method: String,
    pub class: String,
    pub name: String,
    pub holder: String,
    pub held: String,
}

/// `zend_visibility_string`.
fn visibility_word(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Public => "public",
        Visibility::Protected => "protected",
        Visibility::Private => "private",
    }
}

/// The `" or weaker"` php adds after any visibility but public.
fn or_weaker(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Public => "",
        _ => " or weaker",
    }
}

/// `zend_get_object_type`: what a declaration is, lowercase, as php's
/// messages name it.
#[must_use]
pub fn object_type(kind: ClassKind) -> &'static str {
    match kind {
        ClassKind::Class => "class",
        ClassKind::Interface => "interface",
        ClassKind::Trait => "trait",
        ClassKind::Enum => "enum",
    }
}

fn object_type_uc(kind: ClassKind) -> &'static str {
    match kind {
        ClassKind::Class => "Class",
        ClassKind::Interface => "Interface",
        ClassKind::Trait => "Trait",
        ClassKind::Enum => "Enum",
    }
}

impl Refusal {
    /// The level php raises the refusal at: `E_COMPILE_ERROR` for all
    /// but the `E_ERROR` of `zend_verify_abstract_class`, of a class in
    /// an interface list and of the `interface_gets_implemented` guards
    /// of `Throwable` and the enum interfaces, and
    /// `zend_implement_traversable`'s `E_CORE_ERROR`, which php raises
    /// with no site at all.
    #[must_use]
    pub fn level(&self) -> Level {
        match self {
            Refusal::AbstractLeft { .. }
            | Refusal::NotAnInterface { .. }
            | Refusal::Throwable { .. }
            | Refusal::EnumInterface { .. }
            | Refusal::NonBackedEnumInterface { .. }
            | Refusal::IteratorAndAggregate { .. }
            | Refusal::UserImplementor { .. } => Level::Error,
            Refusal::Traversable { .. } => Level::CoreError,
            _ => Level::CompileError,
        }
    }
}

impl fmt::Display for Refusal {
    #[expect(clippy::too_many_lines, reason = "one arm per sentence php raises")]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::InterfaceExtendsClass { interface, class } => {
                write!(f, "Interface {interface} cannot extend class {class}")
            }
            Refusal::ExtendsEnum { class, parent } => {
                write!(f, "Class {class} cannot extend enum {parent}")
            }
            Refusal::ExtendsFinal { class, parent } => {
                write!(f, "Class {class} cannot extend final class {parent}")
            }
            Refusal::ExtendsNotAClass {
                class,
                kind,
                parent,
            } => write!(
                f,
                "Class {class} cannot extend {} {parent}",
                object_type(*kind)
            ),
            Refusal::ReadonlyClass {
                class,
                readonly,
                parent,
            } => {
                let (own, theirs) = if *readonly {
                    ("Readonly", "non-readonly")
                } else {
                    ("Non-readonly", "readonly")
                };
                write!(
                    f,
                    "{own} class {class} cannot extend {theirs} class {parent}"
                )
            }
            Refusal::FinalProperty { parent, property } => {
                write!(f, "Cannot override final property {parent}::${property}")
            }
            Refusal::PropertyReceiver {
                parent,
                class,
                property,
                parent_static,
            } => {
                let (theirs, own) = if *parent_static {
                    ("static ", "non static ")
                } else {
                    ("non static ", "static ")
                };
                write!(
                    f,
                    "Cannot redeclare {theirs}{parent}::${property} as {own}{class}::${property}"
                )
            }
            Refusal::ReadonlyProperty {
                parent,
                class,
                property,
                parent_readonly,
            } => {
                let (theirs, own) = if *parent_readonly {
                    ("readonly", "non-readonly")
                } else {
                    ("non-readonly", "readonly")
                };
                write!(
                    f,
                    "Cannot redeclare {theirs} property {parent}::${property} as {own} {class}::${property}"
                )
            }
            Refusal::SetVisibility {
                class,
                property,
                parent,
                parent_set,
            } => {
                let (word, weaker) = match parent_set {
                    Some(Visibility::Private) => ("private(set)", " or weaker"),
                    Some(Visibility::Protected) => ("protected(set)", " or weaker"),
                    Some(Visibility::Public) => ("public(set)", " or weaker"),
                    None => ("omitted", ""),
                };
                write!(
                    f,
                    "Set access level of {class}::${property} must be {word} (as in class {parent}){weaker}"
                )
            }
            Refusal::PropertyVisibility {
                class,
                property,
                parent,
                visibility,
            } => write!(
                f,
                "Access level to {class}::${property} must be {} (as in class {parent}){}",
                visibility_word(*visibility),
                or_weaker(*visibility)
            ),
            Refusal::PropertyType {
                class,
                property,
                ty,
                parent,
                variance,
            } => write!(
                f,
                "Type of {class}::${property} must be {}{ty} (as in class {parent})",
                match variance {
                    Variance::Invariant => "",
                    Variance::Covariant => "subtype of ",
                    Variance::Contravariant => "supertype of ",
                }
            ),
            Refusal::PropertyTypeOmitted {
                class,
                property,
                parent,
            } => write!(
                f,
                "Type of {class}::${property} must be omitted to match the parent definition in class {parent}"
            ),
            Refusal::SetHookType {
                class,
                property,
                ty,
                parent_kind,
                parent,
            } => write!(
                f,
                "Set type of {class}::${property} must be supertype of {ty} (as in {} {parent})",
                object_type(*parent_kind)
            ),
            Refusal::SetHookParameter {
                parameter,
                class,
                property,
            } => write!(
                f,
                "Type of parameter ${parameter} of hook {class}::${property}::set must be compatible with property type"
            ),
            Refusal::FinalHook { parent, hook } => {
                write!(f, "Cannot override final property hook {parent}::{hook}()")
            }
            Refusal::VirtualDefault { class, property } => write!(
                f,
                "Cannot specify default value for virtual hooked property {class}::${property}"
            ),
            Refusal::BackedGetByRef { class, property } => write!(
                f,
                "Get hook of backed property {class}::{property} with set hook may not return by reference"
            ),
            Refusal::AbstractWithoutAbstractHook { class, property } => write!(
                f,
                "Abstract property {class}::${property} must specify at least one abstract hook"
            ),
            Refusal::VirtualAsymmetric {
                write_only,
                class,
                property,
            } => write!(
                f,
                "{} virtual property {class}::${property} must not specify asymmetric visibility",
                if *write_only {
                    "Write-only"
                } else {
                    "Read-only"
                }
            ),
            Refusal::FinalConstant {
                holder,
                parent,
                constant,
            } => write!(
                f,
                "{holder}::{constant} cannot override final constant {parent}::{constant}"
            ),
            Refusal::AmbiguousConstant {
                kind,
                class,
                holder,
                parent,
                constant,
            } => write!(
                f,
                "{} {class} inherits both {holder}::{constant} and {parent}::{constant}, which is ambiguous",
                object_type_uc(*kind)
            ),
            Refusal::ConstantVisibility {
                class,
                constant,
                visibility,
                parent_kind,
                parent,
            } => write!(
                f,
                "Access level to {class}::{constant} must be {} (as in {} {parent}){}",
                visibility_word(*visibility),
                object_type(*parent_kind),
                or_weaker(*visibility)
            ),
            Refusal::FinalMethod { parent, method } => {
                write!(f, "Cannot override final method {parent}::{method}()")
            }
            Refusal::ConstantType {
                holder,
                constant,
                parent,
                ty,
            } => write!(
                f,
                "Type of {holder}::{constant} must be compatible with {parent}::{constant} of type {ty}"
            ),
            Refusal::MethodReceiver {
                parent,
                method,
                class,
                parent_static,
            } => {
                let (theirs, own) = if *parent_static {
                    ("static", "non static")
                } else {
                    ("non static", "static")
                };
                write!(
                    f,
                    "Cannot make {theirs} method {parent}::{method}() {own} in class {class}"
                )
            }
            Refusal::AbstractMethod {
                parent,
                method,
                class,
            } => write!(
                f,
                "Cannot make non abstract method {parent}::{method}() abstract in class {class}"
            ),
            Refusal::MethodVisibility {
                class,
                method,
                visibility,
                parent,
            } => write!(
                f,
                "Access level to {class}::{method}() must be {} (as in class {parent}){}",
                visibility_word(*visibility),
                or_weaker(*visibility)
            ),
            Refusal::Incompatible { child, parent } => {
                write!(f, "Declaration of {child} must be compatible with {parent}")
            }
            Refusal::Unavailable {
                child,
                parent,
                missing,
            } => write!(
                f,
                "Could not check compatibility between {child} and {parent}, because class {missing} is not available"
            ),
            Refusal::PreviouslyImplemented {
                kind,
                class,
                interface,
            } => write!(
                f,
                "{} {class} cannot implement previously implemented interface {interface}",
                object_type_uc(*kind)
            ),
            Refusal::Throwable { kind, class } => {
                let advice = if *kind == ClassKind::Enum {
                    ""
                } else {
                    ", extend Exception or Error instead"
                };
                write!(
                    f,
                    "{} {class} cannot implement interface Throwable{advice}",
                    object_type_uc(*kind)
                )
            }
            Refusal::NotAnInterface { class, interface } => {
                write!(
                    f,
                    "{class} cannot implement {interface} - it is not an interface"
                )
            }
            Refusal::Traversable { kind, class } => write!(
                f,
                "{} {class} must implement interface Traversable as part of either Iterator or IteratorAggregate",
                object_type_uc(*kind)
            ),
            Refusal::IteratorAndAggregate { class } => write!(
                f,
                "Class {class} cannot implement both Iterator and IteratorAggregate at the same time"
            ),
            Refusal::UserImplementor { interface } => {
                write!(f, "{interface} can't be implemented by user classes")
            }
            Refusal::EnumMagic { class, method } => {
                write!(f, "Enum {class} cannot include magic method {method}")
            }
            Refusal::EnumRedeclare { class, method } => {
                write!(f, "Cannot redeclare {class}::{method}()")
            }
            Refusal::EnumSerializable { class } => {
                write!(
                    f,
                    "Enum {class} cannot implement the Serializable interface"
                )
            }
            Refusal::EnumInterface { class, interface } => {
                write!(
                    f,
                    "Non-enum class {class} cannot implement interface {interface}"
                )
            }
            Refusal::NonBackedEnumInterface { class, interface } => {
                write!(
                    f,
                    "Non-backed enum {class} cannot implement interface {interface}"
                )
            }
            Refusal::AbstractLeft {
                kind,
                class,
                methods,
                must_implement,
            } => {
                let count = methods.len();
                let plural = if count > 1 { "s" } else { "" };
                let named = methods
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = if count > 3 { ", ..." } else { "" };
                let what = object_type_uc(*kind);
                if *must_implement {
                    return write!(
                        f,
                        "{what} {class} must implement {count} abstract method{plural} ({named}{more})"
                    );
                }
                write!(
                    f,
                    "{what} {class} contains {count} abstract method{plural} and must therefore be declared abstract or implement the remaining method{plural} ({named}{more})"
                )
            }
            Refusal::TraitCollision(collision) => {
                let Collision {
                    from,
                    method,
                    class,
                    name,
                    holder,
                    held,
                } = &**collision;
                write!(
                    f,
                    "Trait method {from}::{method} has not been applied as {class}::{name}, because of collision with {holder}::{held}"
                )
            }
            Refusal::TraitNotFound { name } => write!(f, "Could not find trait {name}"),
            Refusal::NotATrait { class } => write!(
                f,
                "Class {class} is not a trait, Only traits may be used in 'as' and 'insteadof' statements"
            ),
            Refusal::TraitNotUsed { name, class } => {
                write!(f, "Required Trait {name} wasn't added to {class}")
            }
            Refusal::PrecedenceMissing { name, method } => write!(
                f,
                "A precedence rule was defined for {name}::{method} but this method does not exist"
            ),
            Refusal::ExcludedTwice { method, name } => write!(
                f,
                "Failed to evaluate a trait precedence ({method}). Method of trait {name} was defined to be excluded multiple times"
            ),
            Refusal::InsteadofItself { method, name } => write!(
                f,
                "Inconsistent insteadof definition. The method {method} is to be used from {name}, but {name} is also on the exclude list"
            ),
            Refusal::AliasMissing { name, method } => write!(
                f,
                "An alias was defined for {name}::{method} but this method does not exist"
            ),
            Refusal::AliasAmbiguous {
                method,
                first,
                second,
            } => write!(
                f,
                "An alias was defined for method {method}(), which exists in both {first} and {second}. Use {first}::{method} or {second}::{method} to resolve the ambiguity"
            ),
            Refusal::AliasNowhere { alias, method } => write!(
                f,
                "An alias ({alias}) was defined for method {method}(), but this method does not exist"
            ),
            Refusal::ModifiersNowhere { method } => write!(
                f,
                "The modifiers of the trait method {method}() are changed, but this method does not exist. Error"
            ),
            Refusal::TraitConstant {
                first,
                from,
                constant,
                class,
            } => write!(
                f,
                "{first} and {from} define the same constant ({constant}) in the composition of {class}. However, the definition differs and is considered incompatible. Class was composed"
            ),
            Refusal::TraitConstantCase {
                from,
                constant,
                class,
            } => write!(
                f,
                "Cannot use trait {from}, because {from}::{constant} conflicts with enum case {class}::{constant}"
            ),
            Refusal::TraitProperty {
                first,
                from,
                property,
                class,
            } => write!(
                f,
                "{first} and {from} define the same property (${property}) in the composition of {class}. However, the definition differs and is considered incompatible. Class was composed"
            ),
            Refusal::TraitHookedProperty {
                first,
                from,
                property,
                class,
            } => write!(
                f,
                "{first} and {from} define the same hooked property (${property}) in the composition of {class}. Conflict resolution between hooked properties is currently not supported. Class was composed"
            ),
            Refusal::ReadonlyTraitProperty {
                class,
                from,
                property,
            } => write!(
                f,
                "Readonly class {class} cannot use trait with a non-readonly property {from}::${property}"
            ),
            Refusal::EnumProperties { class } => {
                write!(f, "Enum {class} cannot include properties")
            }
            Refusal::OverrideMethod { class, method } => write!(
                f,
                "{class}::{method}() has #[\\Override] attribute, but no matching parent method exists"
            ),
            Refusal::OverrideProperty { class, property } => write!(
                f,
                "{class}::${property} has #[\\Override] attribute, but no matching parent property exists"
            ),
        }
    }
}

/// What a link refused and where php raises it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub refusal: Refusal,
    pub site: Site,
}

/// An override of a tentative return type that does not match it,
/// `emit_incompatible_method_error` under `INHERITANCE_WARNING`: php's
/// deprecation, which the child's `#[\ReturnTypeWillChange]` silences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TentativeReturn {
    pub child: String,
    pub parent: String,
    pub site: Site,
}

impl fmt::Display for TentativeReturn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Return type of {} should either be compatible with {}, or the #[\\ReturnTypeWillChange] attribute should be used to temporarily suppress the notice",
            self.child, self.parent
        )
    }
}

/// What a link raised that is no refusal, in the order raised: php
/// records each while the class links (`zend_begin_record_errors`) and
/// emits them before a fatal or once the class stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Raised {
    /// A tentative return overridden out of step, `E_DEPRECATED`.
    TentativeReturn(TentativeReturn),
    /// `zend_traits_check_private_final_inheritance`'s
    /// `E_COMPILE_WARNING`, `Private methods cannot be final as they are
    /// never overridden by other classes`, of an `as` that made a trait's
    /// method both.
    PrivateFinal(Site),
    /// `zend_implement_serializable`'s `E_DEPRECATED` of a class that
    /// implements `Serializable` without `__serialize()` and
    /// `__unserialize()`.
    Serializable { class: String, site: Site },
}

impl Raised {
    /// The level php raises it at.
    #[must_use]
    pub fn level(&self) -> Level {
        match self {
            Raised::TentativeReturn(_) | Raised::Serializable { .. } => Level::Deprecated,
            Raised::PrivateFinal(_) => Level::CompileWarning,
        }
    }

    /// Where php raises it.
    #[must_use]
    pub fn site(&self) -> &Site {
        match self {
            Raised::TentativeReturn(tentative) => &tentative.site,
            Raised::PrivateFinal(site) | Raised::Serializable { site, .. } => site,
        }
    }
}

impl fmt::Display for Raised {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Raised::TentativeReturn(tentative) => tentative.fmt(f),
            Raised::PrivateFinal(_) => f.write_str(
                "Private methods cannot be final as they are never overridden by other classes",
            ),
            Raised::Serializable { class, .. } => write!(
                f,
                "{class} implements the Serializable interface, which is deprecated. Implement __serialize() and __unserialize() instead (or in addition, if support for old PHP versions is necessary)"
            ),
        }
    }
}

/// Where the op array of a method is: the class that declared it with a
/// body, by name, and the method's index among that class's own
/// declared methods. A method a trait gave a class runs the trait's
/// op array, which php shares between the copies
/// (`zend_add_trait_method` copies the `zend_op_array` and not its
/// opcodes), so two methods of one body are one method to
/// `zend_add_trait_method`'s check of a trait used twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body {
    pub class: Name,
    pub method: u32,
}

/// A class as a table holds it for the link: its declaration, where
/// its methods' bodies are when composition put another class's there,
/// one [`Body`] per method in order, none for a class whose every
/// method is its own; and whether it is `ZEND_ACC_LINKED`, which a
/// class the table holds while its own link is under way is not
/// (`ZEND_FETCH_CLASS_ALLOW_UNLINKED`); and for a linked class the
/// caller keeps the record of, the key it names the class by and its record, which a
/// child's link starts from.
#[derive(Debug, Clone, Copy)]
pub struct Held<'a, H = ()> {
    pub decl: &'a ClassDecl,
    pub bodies: Option<&'a [Body]>,
    pub linked: bool,
    pub record: Option<(H, &'a LinkedClass<H>)>,
}

impl<'a, H> Held<'a, H> {
    /// The class as a lookup that reads no record holds it.
    pub(crate) fn plain(self) -> Held<'a> {
        Held {
            decl: self.decl,
            bodies: self.bodies,
            linked: self.linked,
            record: None,
        }
    }
}

impl<H> Held<'_, H> {
    /// The body of the class's method at `at`.
    #[must_use]
    pub(crate) fn body(&self, at: usize) -> Body {
        match self.bodies.and_then(|bodies| bodies.get(at)) {
            Some(body) => body.clone(),
            None => Body {
                class: self.decl.name.clone(),
                method: u32::try_from(at).unwrap_or(u32::MAX),
            },
        }
    }
}

/// A class composed with the traits it uses, what `zend_do_bind_traits`
/// leaves in its tables: the declaration with the traits' methods,
/// constants and properties as its own, in php's order, and the body of
/// each of its methods; and how many of its methods, its last, are the
/// traits' abstract methods it took, which php binds after the parent's
/// (`zend_do_traits_method_binding`'s second pass), so its function
/// table holds them after the parent's.
#[derive(Debug, Clone, PartialEq)]
pub struct Composed {
    pub decl: ClassDecl,
    pub bodies: Vec<Body>,
    pub abstracts: u32,
}

/// What linking one class came to, `zend_do_link_class`'s outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Linked {
    /// The class links: every check held, or left an obligation.
    Linked,
    /// A check failed, php's `E_COMPILE_ERROR` at the site named.
    Refused(Refused),
}

/// A variance obligation, what `zend_do_link_class` leaves on a class
/// it cannot finish linking yet (`variance_obligation`), in the order
/// the checks left them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Obligation {
    /// `OBLIGATION_DEPENDENCY`: the parent or an interface, named, is
    /// not linked yet, and its own obligations are settled first.
    Dependency(Name),
    /// A check that came to `INHERITANCE_UNRESOLVED`, a class it names
    /// held by no table, with the refusal php raises if it still does
    /// when the obligation is checked: `Could not check compatibility`
    /// for a method, and the check's own sentence for a property, a
    /// hook or a constant.
    Unresolved(Refused),
}

/// [`link`]'s answer: the outcome; what the checks raised before it in
/// the order raised; for a class that uses traits and links, the class
/// composed with them; the obligations left, which a bind at load
/// reads as `zend_can_early_bind`'s `INHERITANCE_UNRESOLVED`, and each
/// class a check could not find, in the order
/// `register_unresolved_classes` puts them in `CG(delayed_autoloads)`;
/// and the class's record, where it links and every class it names is
/// held with a record ([`linked_class`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Linking<H = ()> {
    pub outcome: Linked,
    pub raised: Vec<Raised>,
    pub composed: Option<Composed>,
    pub obligations: Vec<Obligation>,
    pub autoloads: Vec<Vec<u8>>,
    pub record: Option<LinkedClass<H>>,
}

impl<H> Linking<H> {
    /// A link refused before any check left anything for later, with
    /// what was raised before the refusal.
    fn refused(refused: Refused, raised: Vec<Raised>) -> Linking<H> {
        Linking {
            outcome: Linked::Refused(refused),
            raised,
            composed: None,
            obligations: Vec::new(),
            autoloads: Vec::new(),
            record: None,
        }
    }
}

/// What the checks left for later, `CG(delayed_autoloads)`'s names and
/// the class's obligations in the order left. A check reads the table
/// and leaves these, so they are held apart from what it reads.
#[derive(Default)]
struct Deferred {
    autoloads: Vec<Vec<u8>>,
    obligations: Vec<Obligation>,
    /// The classes the method check under way registered, the first of
    /// which its `Could not check compatibility` names.
    missing: Vec<Vec<u8>>,
}

/// `inheritance_status`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Success,
    Error,
    Unresolved,
    Warning,
}

/// A class as `lookup_class` answers it while one is being linked: the
/// class being linked, which no table holds yet, or one the table does.
#[derive(Clone, Copy)]
enum Cls<'a> {
    Linking,
    Linked(&'a ClassDecl),
}

impl PartialEq for Cls<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Cls::Linking, Cls::Linking) => true,
            (Cls::Linked(a), Cls::Linked(b)) => same(a, b),
            _ => false,
        }
    }
}

/// One class of the table, by name, whatever the case.
fn same(a: &ClassDecl, b: &ClassDecl) -> bool {
    a.name.0.eq_ignore_ascii_case(&b.name.0)
}

/// The scope a type is read in, `fe_scope` or `proto_scope`: the class
/// that declared the member, which `self` and `parent` resolve against.
#[derive(Clone, Copy)]
struct Scope<'a> {
    name: &'a [u8],
    parent: Option<&'a [u8]>,
    class: Cls<'a>,
    is_final: bool,
}

/// One member of a type's list as `zend_type` holds it once compiled.
#[derive(Clone)]
enum Member {
    Name(Vec<u8>),
    Intersection(Vec<Vec<u8>>),
}

/// A type in `zend_type`'s shape: whether it is a pure intersection,
/// its class names in order, and its pure mask.
struct Shape {
    intersection: bool,
    members: Vec<Member>,
    mask: u32,
}

/// `resolve_class_name`: `self` and `parent` in the scope's names.
fn resolve(scope: &Scope<'_>, name: &[u8]) -> Vec<u8> {
    if name.eq_ignore_ascii_case(b"self") {
        return scope.name.to_vec();
    }
    if name.eq_ignore_ascii_case(b"parent")
        && let Some(parent) = scope.parent
    {
        return parent.to_vec();
    }
    name.to_vec()
}

/// A declared type with `self` and `parent` named, which is what
/// `zend_type_to_string_resolved` prints.
fn resolved(ty: &Type, scope: &Scope<'_>) -> Type {
    match ty {
        Type::SelfClass => Type::Named(Name(scope.name.into())),
        Type::ParentClass => match scope.parent {
            Some(parent) => Type::Named(Name(parent.into())),
            None => Type::ParentClass,
        },
        Type::Nullable(inner) => Type::Nullable(Box::new(resolved(inner, scope))),
        Type::Union(members) => Type::Union(members.iter().map(|m| resolved(m, scope)).collect()),
        Type::Intersection(members) => {
            Type::Intersection(members.iter().map(|m| resolved(m, scope)).collect())
        }
        other => other.clone(),
    }
}

fn names_into(ty: &Type, scope: &Scope<'_>, out: &mut Vec<Member>) {
    match ty {
        Type::Named(name) => out.push(Member::Name(name.0.to_vec())),
        Type::SelfClass => out.push(Member::Name(resolve(scope, b"self"))),
        Type::ParentClass => out.push(Member::Name(resolve(scope, b"parent"))),
        Type::Iterable => out.push(Member::Name(b"Traversable".to_vec())),
        Type::Nullable(inner) => names_into(inner, scope, out),
        Type::Union(members) => {
            for member in members {
                names_into(member, scope, out);
            }
        }
        Type::Intersection(members) => {
            let mut inner = Vec::new();
            for member in members {
                names_into(member, scope, &mut inner);
            }
            out.push(Member::Intersection(
                inner
                    .into_iter()
                    .filter_map(|m| match m {
                        Member::Name(name) => Some(name),
                        Member::Intersection(_) => None,
                    })
                    .collect(),
            ));
        }
        _ => {}
    }
}

fn shape(ty: &Type, scope: &Scope<'_>) -> Shape {
    if let Type::Intersection(members) = ty {
        let mut names = Vec::new();
        for member in members {
            names_into(member, scope, &mut names);
        }
        return Shape {
            intersection: true,
            members: names,
            mask: 0,
        };
    }
    let mut members = Vec::new();
    names_into(ty, scope, &mut members);
    Shape {
        intersection: false,
        members,
        mask: ty.mask(),
    }
}

/// `ZEND_ACC_PPP_MASK` ordered as php's bits are, public the least.
fn strength(visibility: Visibility) -> u8 {
    match visibility {
        Visibility::Public => 0,
        Visibility::Protected => 1,
        Visibility::Private => 2,
    }
}

/// A name for a message, as php's `%s` prints it ([`php_unit::decl::shown`]).
fn text(bytes: &[u8]) -> String {
    php_unit::decl::shown(bytes).into_owned()
}

/// `required_num_args`: the parameters up to the last one with no
/// default that is not variadic.
fn required(function: &FunctionDecl) -> usize {
    function
        .parameters
        .iter()
        .rposition(Parameter::is_required)
        .map_or(0, |at| at + 1)
}

fn is_variadic(function: &FunctionDecl) -> bool {
    function.parameters.last().is_some_and(|p| p.variadic)
}

/// A default as `zend_get_function_declaration` prints it.
///
/// A float is written at php's `precision` of 14 in the `E` form:
///
/// ```php
/// class B { function t($x = 3.141592653589793, $y = 1.0, $z = 1e100) {} }
/// class S extends B { function t() {} }
/// ```
/// ```expect exit=255
/// Fatal error: Declaration of S::t() must be compatible with B::t($x = 3.1415926535898, $y = 1, $z = 1.0E+100) in %s on line 3
/// ```
///
/// An array is `[]` or `[...]` where the compiler folded it, and an
/// `<expression>` where an element is still to evaluate:
///
/// ```php
/// class B { function t($x = [FOO], $y = [1], $z = [], $w = [[]]) {} }
/// class S extends B { function t() {} }
/// ```
/// ```expect exit=255
/// Fatal error: Declaration of S::t() must be compatible with B::t($x = <expression>, $y = [...], $z = [], $w = [...]) in %s on line 3
/// ```
fn default_text(default: &ConstExpr) -> String {
    match default {
        ConstExpr::Null => String::from("null"),
        ConstExpr::Bool(false) => String::from("false"),
        ConstExpr::Bool(true) => String::from("true"),
        ConstExpr::Long { value, radix } => radix.written(*value),
        // `smart_str_append_double(&str, d, EG(precision), false)`, at
        // php's default `precision` of 14.
        ConstExpr::Double(d) => php_heap::double_to_string(*d, 14, php_heap::Exponent::Upper),
        ConstExpr::Str(bytes) => {
            let shown = &bytes[..bytes.len().min(10)];
            let more = if bytes.len() > 10 { "..." } else { "" };
            format!("'{}{more}'", String::from_utf8_lossy(shown))
        }
        // An array the compiler folded is a value, `[]` or `[...]`; one
        // still to evaluate is the `<expression>` below, as php prints
        // a `ZEND_AST_ARRAY`, and a native's empty one is its stub's `[]`.
        ConstExpr::UnitArray(array) if array.is_empty() => String::from("[]"),
        ConstExpr::UnitArray(_) => String::from("[...]"),
        ConstExpr::Array { elements, .. } if elements.is_empty() => String::from("[]"),
        ConstExpr::Constant { name, .. } => text(&name.resolved().0),
        ConstExpr::ClassConstant { class, name, .. } => {
            format!("{}::{}", text(&class.0), text(&name.0))
        }
        // A native's `C::class` as its stub writes it; a program's
        // `self::class` is none of the nodes php prints.
        ConstExpr::ClassName { class, .. }
            if !class.0.eq_ignore_ascii_case(b"self")
                && !class.0.eq_ignore_ascii_case(b"parent") =>
        {
            format!("{}::class", text(&class.0))
        }
        ConstExpr::EnumCase { class, case, .. } => format!("{}::{}", text(&class.0), text(&case.0)),
        ConstExpr::ScopeClass => String::from("__CLASS__"),
        _ => String::from("<expression>"),
    }
}

/// `zend_get_function_declaration`: a method as php writes it in an
/// incompatibility, its own scope's name before its name and its types
/// read in the side's scope.
fn declaration(function: &FunctionDecl, side: &Side<'_>) -> String {
    let scope = &side.scope;
    let mut out = String::new();
    if function.returns_by_ref {
        out.push_str("& ");
    }
    out.push_str(&text(side.shown));
    out.push_str("::");
    out.push_str(&text(&function.name.0));
    out.push('(');
    let required = required(function);
    for (at, parameter) in function.parameters.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        if let Some(ty) = &parameter.ty {
            out.push_str(&type_to_string(&resolved(ty, scope)));
            out.push(' ');
        }
        if parameter.by_ref {
            out.push('&');
        }
        if parameter.variadic {
            out.push_str("...");
        }
        out.push('$');
        out.push_str(&text(&parameter.name.0));
        if at >= required && !parameter.variadic {
            out.push_str(" = ");
            out.push_str(
                &parameter
                    .default
                    .expr()
                    .map_or_else(|| String::from("<default>"), default_text),
            );
        }
    }
    out.push(')');
    if let Some(ty) = &function.return_type {
        out.push_str(": ");
        out.push_str(&type_to_string(&resolved(ty, scope)));
    }
    out
}

/// One side of `do_inheritance_check_on_method`: the scope its types
/// resolve in, `fixup_trait_scope`'s answer, and the class
/// `ZEND_FN_SCOPE_NAME` prints for it, which for a trait's method not
/// yet fixed up is the trait.
#[derive(Clone, Copy)]
struct Side<'a> {
    scope: Scope<'a>,
    shown: &'a [u8],
}

/// Which of php's calls of `do_inheritance_check_on_method` a check is:
/// a parent's method, an interface's, or a trait's abstract method held
/// against the one the class has (`zend_add_trait_method`), which
/// `ZEND_INHERITANCE_CHECK_VISIBILITY` is not passed to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Check {
    Parent,
    Interface,
    TraitAbstract,
}

/// A kind of member a class declares and its linked record keeps a
/// table of: a method, a constant or a property. The walks over those
/// tables differ only in which table they read, and this says which.
trait ClassMember: Sized + 'static {
    /// How the table's names compare.
    type Case: Case;
    /// The members of this kind `class` declares, in order.
    fn of(class: &ClassDecl) -> &[Self];
    /// The member's name.
    fn name(&self) -> &[u8];
    /// The record's table of this kind.
    fn table<H>(record: &LinkedClass<H>) -> &Rows<Row<H>, Self::Case>;
    /// Whether a class below the one that declared it holds it: all but
    /// a private constant (`do_inherit_constant_check`), since
    /// `do_inherit_method` and `do_inherit_property` append a private
    /// method or property too.
    fn inherited(&self) -> bool {
        true
    }
    /// Whether an internal class's own come after what it inherits: a
    /// constant, which the stub's register function declares after
    /// `zend_register_internal_class_ex` inherited the parent's table
    /// and `zend_class_implements` the interfaces'; a method is
    /// registered with the class, before either.
    const LAST_WHEN_INTERNAL: bool = false;
    /// Whether an internal class's own that redeclares an inherited one
    /// takes its place at the end, `zend_hash_del` then
    /// `zend_hash_update_ptr`: a property (`zend_declare_typed_property`).
    const REDECLARED_LAST: bool = false;
}

impl ClassMember for MethodDecl {
    type Case = Folded;
    fn of(class: &ClassDecl) -> &[Self] {
        &class.methods
    }
    fn name(&self) -> &[u8] {
        &self.function.name.0
    }
    fn table<H>(record: &LinkedClass<H>) -> &Rows<Row<H>, Folded> {
        &record.methods
    }
}

impl ClassMember for ClassConstDecl {
    type Case = Exact;
    const LAST_WHEN_INTERNAL: bool = true;
    fn of(class: &ClassDecl) -> &[Self] {
        &class.constants
    }
    fn name(&self) -> &[u8] {
        &self.name.0
    }
    fn table<H>(record: &LinkedClass<H>) -> &Rows<Row<H>, Exact> {
        &record.constants
    }
    fn inherited(&self) -> bool {
        self.visibility != Visibility::Private
    }
}

impl ClassMember for PropertyDecl {
    type Case = Exact;
    const LAST_WHEN_INTERNAL: bool = true;
    const REDECLARED_LAST: bool = true;
    fn of(class: &ClassDecl) -> &[Self] {
        &class.properties
    }
    fn name(&self) -> &[u8] {
        &self.name.0
    }
    fn table<H>(record: &LinkedClass<H>) -> &Rows<Row<H>, Exact> {
        &record.properties
    }
}

/// A table of one kind a class holds as it links, each member with the
/// class that put it there, none for the class's own, indexed by name.
type Members<'a, M> = Rows<(Option<&'a ClassDecl>, &'a M), <M as ClassMember>::Case>;

/// The function table a class holds as it links.
type Methods<'a> = Members<'a, MethodDecl>;

/// The constant table a class holds as it links.
type Constants<'a> = Members<'a, ClassConstDecl>;

/// A member of [`Members`] by its name.
fn member_name<'a, M: ClassMember>(row: &(Option<&'a ClassDecl>, &'a M)) -> &'a [u8] {
    row.1.name()
}

/// `member`, which `by` put there, none for the class's own, into
/// `table` under its name, unless one of its name stands already.
fn put<'a, M: ClassMember>(table: &mut Members<'a, M>, by: Option<&'a ClassDecl>, member: &'a M) {
    table.add((by, member), member.name(), member_name);
}

/// The class being linked, the table it is linked against and the
/// class each key of a record's rows names, what was raised so far
/// and what the checks left for later.
struct Linker<'a, H = ()> {
    lookup: &'a dyn Fn(&[u8]) -> Option<Held<'a, H>>,
    holder: &'a dyn Fn(H) -> Option<&'a ClassDecl>,
    standing: Standing,
    raised: Vec<Raised>,
    deferred: std::cell::RefCell<Deferred>,
}

/// Where the class being linked stands while it links, which is what
/// `lookup_class` finds of it by its name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// In the table under its name, as `do_bind_class` put it before
    /// `zend_do_link_class`: every lookup of the name finds it.
    Declared,
    /// In no table yet, as at `zend_try_early_bind` while the file
    /// compiles, which `register_early_bound_ce` adds it to only once
    /// `zend_can_early_bind` has passed: a type its own members write
    /// finds it, by `lookup_class_ex`'s check of the scope's name, and
    /// one its parent's write does not.
    Compiling,
}

/// `zend_do_link_class` over `child` and the classes `lookup` finds by
/// name, its traits, its parent and its interfaces among them, a row
/// of a held record naming its class by a key `holder` reads: the
/// traits bound first ([`traits`]), then every refusal
/// `zend_do_inheritance_ex` and `do_interface_implementation` make, the
/// first in php's order; and the class's record where it links.
pub fn link<'a, H: Copy + PartialEq>(
    child: &'a ClassDecl,
    lookup: &'a dyn Fn(&[u8]) -> Option<Held<'a, H>>,
    holder: &'a dyn Fn(H) -> Option<&'a ClassDecl>,
    values: &'a dyn Fn(&ConstExpr) -> Option<ConstExpr>,
) -> Linking<H> {
    if let Some(refused) = enum_funcs_refused(child) {
        return Linking::refused(refused, Vec::new());
    }
    if child.traits.is_empty() {
        return Linker::over(lookup, holder, Vec::new()).link_plain(child);
    }
    let plain = |name: &[u8]| lookup(name).map(Held::plain);
    let composition = match traits::compose(child, &plain, values) {
        Ok(composition) => composition,
        Err(failed) => {
            let (refused, raised) = *failed;
            return Linking::refused(refused, raised);
        }
    };
    let traits::Composition {
        mut decl,
        mut bodies,
        abstracts,
        raised,
    } = composition;
    let (settled, raised, deferred) = link_composed(
        &|name| lookup(name),
        &|h| holder(h),
        &decl,
        &abstracts,
        raised,
    );
    let taken = settled.added.len();
    for at in settled.added {
        if let Some(copy) = abstracts.get(at) {
            decl.methods.push(copy.method.clone());
            bodies.push(copy.body.clone());
        }
    }
    if settled.stringable {
        decl.interfaces.push(Name::from("Stringable"));
    }
    let record = (settled.outcome == Linked::Linked)
        .then(|| linked_class(&decl, taken, &|name| lookup(name), &|h| holder(h)))
        .flatten();
    let composed = (settled.outcome == Linked::Linked).then_some(Composed {
        decl,
        bodies,
        abstracts: u32::try_from(taken).unwrap_or(u32::MAX),
    });
    Linking {
        outcome: settled.outcome,
        raised,
        composed,
        obligations: deferred.obligations,
        autoloads: deferred.autoloads,
        record,
    }
}

/// `zend_enum_register_funcs`, which `zend_do_link_class` runs over an
/// enum once its parent, traits and interfaces are fetched and before
/// it binds a trait: `cases`, and for a backed enum `from` and
/// `tryfrom`, added to its function table, where a method of its own
/// under the name is php's `Cannot redeclare`, raised as the enum
/// links, at its declaration, the name spelled as php's known string
/// spells it, lowered. The declaration holds the enum's own methods and
/// after them the rows [`php_unit::enum_methods`] gives every enum, so
/// a row's name held twice is `zend_hash_add_ptr` failing.
///
/// ```php
/// echo "before\n";
/// enum Foo: int {
///     case Bar = 0;
///     public static function tryFrom(int|string $v): ?self { return null; }
/// }
/// ```
/// ```expect exit=255
/// before
///
/// Fatal error: Cannot redeclare Foo::tryfrom() in %s on line 3
/// ```
///
/// A pure enum is given no `from`, so one of its own is its own:
///
/// ```php
/// enum Foo { case Bar; public function from() { return 1; } }
/// var_dump(Foo::Bar->from());
/// ```
/// ```expect
/// int(1)
/// ```
fn enum_funcs_refused(child: &ClassDecl) -> Option<Refused> {
    if child.kind != ClassKind::Enum {
        return None;
    }
    let held = |given: &MethodDecl| {
        child
            .methods
            .iter()
            .filter(|m| same_method(m, given))
            .count()
    };
    let given = php_unit::enum_methods(child.backing);
    let clash = given.iter().find(|given| held(given) > 1)?;
    Some(at_class(
        child,
        Refusal::EnumRedeclare {
            class: text(&child.name.0),
            method: text(&clash.function.name.0.to_ascii_lowercase()),
        },
    ))
}

/// `zend_try_early_bind` over `child`, a class with no trait, while the
/// file compiles: [`link`] against the classes `lookup` finds, with the
/// class itself in no table yet ([`Standing::Compiling`]), so a check
/// that needs it from a scope other than its own comes to
/// `INHERITANCE_UNRESOLVED` and leaves an obligation, where php keeps
/// the class for its `DECLARE_CLASS_DELAYED`.
///
/// ```php
/// class A { public function m(B|string $a): A|string { return 'A'; } }
/// class B extends A { public function m(A|string $a): B|string { return 'B'; } }
/// echo (new B)->m(new B), "\n";
/// ```
/// ```expect
/// B
/// ```
///
/// opcache's load makes the same bind, `zend_accel_do_delayed_early_binding`
/// with the class still under its runtime definition key alone, so the
/// engine's load links a `DECLARE_CLASS_DELAYED` class through here too,
/// over its table's records and the keys `holder` reads.
pub fn early_bind<'a, H: Copy + PartialEq>(
    child: &'a ClassDecl,
    lookup: &'a dyn Fn(&[u8]) -> Option<Held<'a, H>>,
    holder: &'a dyn Fn(H) -> Option<&'a ClassDecl>,
) -> Linking<H> {
    let mut linker = Linker::over(lookup, holder, Vec::new());
    linker.standing = Standing::Compiling;
    linker.link_plain(child)
}

/// The record of `child` once linked, whose last `abstracts` methods
/// are the traits' abstract ones it took ([`Composed::abstracts`]),
/// over the classes `lookup` finds and the keys `holder` reads: none
/// where its parent or an interface is held with no record, which a
/// class under way is. [`link`]'s answer carries it; this is the record
/// alone, of a class whose checks were made elsewhere, at the early
/// bind.
pub fn linked_class<'b, H: Copy + PartialEq>(
    child: &'b ClassDecl,
    abstracts: usize,
    lookup: &dyn Fn(&[u8]) -> Option<Held<'b, H>>,
    holder: &dyn Fn(H) -> Option<&'b ClassDecl>,
) -> Option<LinkedClass<H>> {
    Linker::over(&|name| lookup(name), &|h| holder(h), Vec::new()).linked_class(child, abstracts)
}

/// [`Linker::link_refused`] over a composed declaration, which lives
/// shorter than the table: the lookup narrowed to its lifetime.
fn link_composed<'b, H: Copy>(
    lookup: &'b dyn Fn(&[u8]) -> Option<Held<'b, H>>,
    holder: &'b dyn Fn(H) -> Option<&'b ClassDecl>,
    decl: &'b ClassDecl,
    abstracts: &'b [traits::Copy],
    raised: Vec<Raised>,
) -> (Settled, Vec<Raised>, Deferred) {
    let mut linker = Linker::over(lookup, holder, raised);
    let settled = linker.link_refused(decl, abstracts);
    (settled, linker.raised, linker.deferred.into_inner())
}

/// Whether an internal class names `Stringable` first among its
/// interfaces: one that declares `__toString`, which
/// `zend_register_functions` makes implement it before
/// `zend_class_implements` runs. A user class's compiler appends it to
/// the names the declaration holds instead.
fn names_stringable(class: &ClassDecl) -> bool {
    class.file.0.is_empty()
        && class.kind != ClassKind::Trait
        && !class.name.0.eq_ignore_ascii_case(b"Stringable")
        && class
            .methods
            .iter()
            .any(|m| m.function.name.0.eq_ignore_ascii_case(b"__toString"))
}

/// A class the linker holds with its record: the declaration, the key
/// the record's rows name it by, and the record.
#[derive(Clone, Copy)]
struct Recorded<'a, H> {
    decl: &'a ClassDecl,
    handle: H,
    record: &'a LinkedClass<H>,
}

/// A method of a parent's or an interface's function table, as the
/// prototype rules read it.
#[derive(Clone, Copy)]
struct Above<'a, H> {
    method: &'a MethodDecl,
    scope: H,
    prototype: Option<H>,
}

impl<H: Copy> Above<'_, H> {
    /// The prototype `do_inheritance_check_on_method` gives a method over
    /// this one, the parent's or an interface's method of its name: the
    /// prototype this one has, or its own scope; none where it gives
    /// none, this one private and not abstract, or a constructor whose
    /// prototype is not abstract.
    fn prototype_over(self) -> Option<H> {
        let method = self.method;
        let ctor = method.function.name.0.eq_ignore_ascii_case(b"__construct");
        if method.visibility == Visibility::Private && !method.is_abstract && !ctor {
            return None;
        }
        if ctor && self.prototype.is_none() && !method.is_abstract {
            return None;
        }
        Some(self.prototype.unwrap_or(self.scope))
    }
}

/// What [`Linker::link_refused`] settled: the outcome, which of a
/// trait's abstract methods the class took as its own (indices into the
/// copies it was handed), and whether a trait's `__toString` makes it
/// `Stringable`.
struct Settled {
    outcome: Linked,
    added: Vec<usize>,
    stringable: bool,
}

impl Settled {
    fn of(outcome: Linked) -> Settled {
        Settled {
            outcome,
            added: Vec::new(),
            stringable: false,
        }
    }
}

impl<'a, H: Copy + PartialEq> Linker<'a, H> {
    /// [`link`] of `child`, which uses no trait, by this linker.
    fn link_plain(mut self, child: &'a ClassDecl) -> Linking<H> {
        let settled = self.link_refused(child, &[]);
        let record = (settled.outcome == Linked::Linked)
            .then(|| self.linked_class(child, 0))
            .flatten();
        let deferred = self.deferred.into_inner();
        Linking {
            outcome: settled.outcome,
            raised: self.raised,
            composed: None,
            obligations: deferred.obligations,
            autoloads: deferred.autoloads,
            record,
        }
    }
}

impl<'a, H: Copy> Linker<'a, H> {
    fn over(
        lookup: &'a dyn Fn(&[u8]) -> Option<Held<'a, H>>,
        holder: &'a dyn Fn(H) -> Option<&'a ClassDecl>,
        raised: Vec<Raised>,
    ) -> Linker<'a, H> {
        Linker {
            lookup,
            holder,
            standing: Standing::Declared,
            raised,
            deferred: std::cell::RefCell::default(),
        }
    }

    /// `add_compatibility_obligation` and its siblings: a check that
    /// came to `INHERITANCE_UNRESOLVED` left on the class with the
    /// refusal it raises if it still does when checked, and the link
    /// goes on.
    fn defer(&self, refused: Refused) -> Option<Linked> {
        self.deferred
            .borrow_mut()
            .obligations
            .push(Obligation::Unresolved(refused));
        None
    }

    /// `add_dependency_obligation` where the class `name` names is held
    /// but not linked yet.
    fn depend(&self, name: &Name) {
        if (self.lookup)(&name.0).is_some_and(|held| !held.linked) {
            self.deferred
                .borrow_mut()
                .obligations
                .push(Obligation::Dependency(name.clone()));
        }
    }

    /// `register_unresolved_classes`: each class `ty` names in `scope`
    /// that no table holds, into `CG(delayed_autoloads)` once.
    fn register(&self, child: &'a ClassDecl, ty: &Type, scope: &Scope<'a>) {
        let mut members = Vec::new();
        names_into(ty, scope, &mut members);
        for name in members.into_iter().flat_map(|m| match m {
            Member::Name(name) => vec![name],
            Member::Intersection(names) => names,
        }) {
            if self.lookup(child, scope, &name).is_some() {
                continue;
            }
            let mut deferred = self.deferred.borrow_mut();
            if !deferred.autoloads.contains(&name) {
                deferred.autoloads.push(name.clone());
            }
            deferred.missing.push(name);
        }
    }

    fn class_named(&self, name: &[u8]) -> Option<&'a ClassDecl> {
        (self.lookup)(name).map(|held| held.decl)
    }

    fn class_parent(&self, class: &ClassDecl) -> Option<&'a ClassDecl> {
        class
            .parent
            .as_ref()
            .and_then(|name| self.class_named(&name.0))
    }

    /// `instanceof_function` over the table: `class` is `of`, extends
    /// it or implements it, cut at a depth no declaration makes.
    fn instance_of(&self, class: &ClassDecl, of: &ClassDecl) -> bool {
        self.instance_of_at(class, of, 0)
    }

    fn instance_of_at(&self, class: &ClassDecl, of: &ClassDecl, depth: u32) -> bool {
        if same(class, of) {
            return true;
        }
        if depth > 64 {
            return false;
        }
        class
            .parent
            .iter()
            .chain(class.interfaces.iter())
            .filter_map(|name| self.class_named(&name.0))
            .any(|up| self.instance_of_at(up, of, depth + 1))
    }

    /// `class`'s function table as php holds it once linked, each method
    /// beside the class that declared it: read off its record where the
    /// table holds one, and otherwise, for a class under way or one the
    /// caller keeps no record of, its own methods, then its parent's
    /// table, then the methods of the interfaces it names, which
    /// `do_interface_implementation` put in the table a child inherits.
    /// A composed class's traits' abstract methods count among its own
    /// there, which only the order of a refusal's list could tell:
    ///
    /// ```php
    /// interface I { function __construct(); }
    /// abstract class P implements I {}
    /// class C extends P { function __construct($a) {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Declaration of C::__construct($a) must be compatible with I::__construct() in %s on line 4
    /// ```
    fn methods_above(&self, class: &'a ClassDecl) -> Vec<(&'a ClassDecl, &'a MethodDecl)> {
        let table = self.members_above::<MethodDecl>(class);
        table
            .rows()
            .iter()
            .map(|&(by, method)| (by.unwrap_or(class), method))
            .collect()
    }

    /// `class`'s table of `M` as php holds it once linked, each member
    /// beside the class that declared it: read off its record where the
    /// table holds one, and otherwise, for a class under way or one the
    /// caller keeps no record of ([`Linker::members_into`]), its own,
    /// then its parent's table, then its interfaces', which
    /// `do_interface_implementation` put in the table a child inherits.
    /// A parent's table holds what its interfaces declare, so a class
    /// redeclaring such a property is held to the interface's through
    /// its parent (`zend_do_inheritance`):
    ///
    /// ```php
    /// interface I { public int $x { get; } }
    /// abstract class P implements I {}
    /// class C extends P { protected int $x = 1; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Access level to C::$x must be public (as in class I) in %s on line 4
    /// ```
    fn members_above<M: ClassMember>(&self, class: &'a ClassDecl) -> Members<'a, M> {
        let mut table = Rows::default();
        self.members_into(class, &mut table, 0);
        table
    }

    /// [`Linker::members_above`] into `table`, `depth` above the class
    /// asked: what an ancestor declares and does not pass on
    /// ([`ClassMember::inherited`]) is in no table below it.
    ///
    /// ```php
    /// interface I { final const F = 1; }
    /// class G { private const X = 1; }
    /// class P extends G implements I {}
    /// class C extends P { private const X = 2; }
    /// class D extends P { const F = 2; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: D::F cannot override final constant I::F in %s on line 6
    /// ```
    fn members_into<M: ClassMember>(
        &self,
        class: &'a ClassDecl,
        table: &mut Members<'a, M>,
        depth: u32,
    ) {
        let kept = |member: &M| depth == 0 || member.inherited();
        let held = (self.lookup)(&class.name.0).filter(|held| same(held.decl, class));
        if let Some((_, record)) = held.and_then(|held| held.record) {
            for row in M::table(record).rows() {
                if let Some((by, member)) = self.row_member::<M>(class, row)
                    && kept(member)
                {
                    put(table, Some(by), member);
                }
            }
            return;
        }
        for member in M::of(class).iter().filter(|member| kept(member)) {
            put(table, Some(class), member);
        }
        if depth < 64
            && let Some(parent) = self.class_parent(class)
        {
            self.members_into(parent, table, depth + 1);
        }
        let mut interfaces = Vec::new();
        self.interfaces_of(&class.interfaces, &mut interfaces, 0);
        for interface in interfaces {
            for member in M::of(interface) {
                put(table, Some(interface), member);
            }
        }
    }

    /// [`linked_class`] of `child`, whose last `abstracts` methods are
    /// its traits' abstract ones.
    fn linked_class(&self, child: &'a ClassDecl, abstracts: usize) -> Option<LinkedClass<H>>
    where
        H: PartialEq,
    {
        let own = child.methods.len().saturating_sub(abstracts);
        let methods = self.member_rows::<MethodDecl>(child, own)?;
        let magic = |name: &[u8]| {
            methods
                .find_at(name, |row| self.row_member_name::<MethodDecl>(child, row))
                .and_then(|(at, _)| u32::try_from(at).ok())
        };
        let (constructor, destructor) = (magic(b"__construct"), magic(b"__destruct"));
        let accessors = Accessors {
            get: magic(b"__get"),
            set: magic(b"__set"),
            isset: magic(b"__isset"),
            unset: magic(b"__unset"),
        };
        let properties = self.member_rows::<PropertyDecl>(child, usize::MAX)?;
        let hooks = self.class_hooks(child, &properties);
        Some(LinkedClass {
            prototypes: self.prototypes(child, &methods)?,
            constructor,
            destructor,
            accessors,
            methods,
            constants: self.member_rows::<ClassConstDecl>(child, usize::MAX)?,
            properties,
            hooks,
            interfaces: self.interface_list(child)?,
        })
    }

    /// Where `child`'s property hooks are once its `properties` stand:
    /// [`ClassHooks::Rows`] where a row's `prop_info` has hooks, its own
    /// or ones `inherit_property_hook` left on it, as the compile and
    /// `do_inherit_property` count `ce->num_hooked_props`; a parent's
    /// private hooked property the class redeclares is no row and
    /// counts nothing, but its hooks are still read under that
    /// parent's scope ([`ClassHooks::ParentPrivate`]).
    ///
    /// ```php
    /// class P { private $p = 1 { get => $this->p + 1; } function f() { return $this->p; } }
    /// class Q extends P { private $p = 6; public $r = 2; }
    /// $q = new Q;
    /// var_dump($q->f(), get_object_vars($q));
    /// foreach ($q as $k => $v) echo "$k=$v\n";
    /// echo json_encode($q), "\n";
    /// ```
    /// ```expect
    /// int(2)
    /// array(1) {
    ///   ["r"]=>
    ///   int(2)
    /// }
    /// r=2
    /// {"r":2}
    /// ```
    fn class_hooks(&self, child: &'a ClassDecl, properties: &Rows<Row<H>, Exact>) -> ClassHooks {
        let lookup = |name: &[u8]| (self.lookup)(name).map(Held::plain);
        let rows = properties.rows().iter().any(|row| {
            self.row_member::<PropertyDecl>(child, row)
                .is_some_and(|(by, property)| {
                    property.hooks.is_some()
                        || resolve_property(by, &property.name.0, &lookup)
                            .is_some_and(|resolved| resolved.is_hooked())
                })
        });
        if rows {
            return ClassHooks::Rows;
        }
        let parent = child
            .parent
            .as_ref()
            .and_then(|parent| (self.lookup)(&parent.0))
            .and_then(|held| held.record)
            .map_or(ClassHooks::Absent, |(_, record)| record.hooks);
        match parent {
            ClassHooks::Absent => ClassHooks::Absent,
            ClassHooks::ParentPrivate | ClassHooks::Rows => ClassHooks::ParentPrivate,
        }
    }

    /// `common.prototype`'s scope for each row of `child`'s function
    /// table, as `do_inheritance_check_on_method` leaves it once the link
    /// ran: a method the class declares over its parent's takes the
    /// parent method's prototype, or the parent method itself, unless
    /// that one is private and not abstract, or a constructor whose
    /// prototype is not abstract; an inherited row keeps the prototype it
    /// has in the parent's table, or in the table of the interface it
    /// came from; and each interface the class itself names, the parent's
    /// aside, sets it again where it has a method of the name, a method
    /// it shares with the row aside (`do_interface_implementation` over
    /// `ce->interfaces[num_parent_interfaces..num_interfaces]`, the
    /// named ones before `zend_do_inherit_interfaces` appends their
    /// parents). An internal class names `Stringable` first where it
    /// declares `__toString`. An interface's methods take none, since
    /// `do_interface_implementation` passes an interface no
    /// `ZEND_INHERITANCE_SET_CHILD_PROTO`. None where the parent or an
    /// interface is held with no record.
    ///
    /// ```php
    /// interface I { function m(); function __construct(); }
    /// abstract class P { abstract function a(); private function p() {} function __construct() {} function q() {} }
    /// class C extends P implements I { function m() {} function a() {} function p() {} function __construct() {} }
    /// foreach (['m', 'a', 'p', '__construct', 'q'] as $n) { $r = new ReflectionMethod('C', $n); var_dump($r->hasPrototype() ? $r->getPrototype()->class : null); }
    /// interface J { function q(); }
    /// class E extends P implements J { function a() {} }
    /// var_dump((new ReflectionMethod('E', 'q'))->getPrototype()->class, (new ReflectionMethod('P', 'q'))->hasPrototype());
    /// interface X { function m(); }
    /// interface A extends X { function m(); }
    /// var_dump((new ReflectionMethod('A', 'm'))->hasPrototype());
    /// class D implements A { function m() {} function __toString(): string { return ''; } }
    /// var_dump((new ReflectionMethod('D', 'm'))->getPrototype()->class, (new ReflectionMethod('D', '__toString'))->getPrototype()->class);
    /// var_dump((new ReflectionMethod('Exception', '__toString'))->getPrototype()->class, (new ReflectionMethod('ArrayIterator', 'count'))->getPrototype()->class);
    /// ```
    /// ```expect
    /// string(1) "I"
    /// string(1) "P"
    /// NULL
    /// string(1) "I"
    /// NULL
    /// string(1) "J"
    /// bool(false)
    /// bool(false)
    /// string(1) "A"
    /// string(10) "Stringable"
    /// string(10) "Stringable"
    /// string(9) "Countable"
    /// ```
    fn prototypes(
        &self,
        child: &'a ClassDecl,
        methods: &Rows<Row<H>, Folded>,
    ) -> Option<Box<[Option<H>]>>
    where
        H: PartialEq,
    {
        if child.kind == ClassKind::Interface {
            return Some(vec![None; methods.rows().len()].into_boxed_slice());
        }
        // A class held with a record, none where no class has the name.
        let held = |name: &[u8]| match (self.lookup)(name) {
            None => Some(None),
            Some(held) => held.record.map(|(handle, record)| {
                Some(Recorded {
                    decl: held.decl,
                    handle,
                    record,
                })
            }),
        };
        let parent = match &child.parent {
            Some(name) => held(&name.0)?,
            None => None,
        };
        let inherited = parent.map_or(&[][..], |parent| &*parent.record.interfaces);
        let mut implemented: Vec<Recorded<'a, H>> = Vec::new();
        let names = names_stringable(child)
            .then(|| &b"Stringable"[..])
            .into_iter()
            .chain(child.interfaces.iter().map(|name| &*name.0));
        for name in names {
            if let Some(interface) = held(name)?
                && !inherited.contains(&interface.handle)
                && !implemented
                    .iter()
                    .any(|seen| seen.handle == interface.handle)
            {
                implemented.push(interface);
            }
        }
        let mut prototypes = Vec::with_capacity(methods.rows().len());
        for row in methods.rows() {
            let name = self.row_member_name::<MethodDecl>(child, row);
            let above = parent.and_then(|parent| self.method_above(parent, name));
            let mut prototype = None;
            // A row no parent has came from the first interface that
            // has it, and keeps that one's prototype.
            let mut appended = false;
            match above {
                Some(above) if row.holder.is_some() => prototype = above.prototype,
                Some(above) => prototype = above.prototype_over(),
                None => appended = row.holder.is_some(),
            }
            for &interface in &implemented {
                let Some(declared) = self.method_above(interface, name) else {
                    continue;
                };
                if row.holder.is_some() && row.holder == Some(declared.scope) {
                    // `do_inherit_method`: the same method of an
                    // interface may be inherited a few times.
                    if appended {
                        prototype = declared.prototype;
                        appended = false;
                    }
                    continue;
                }
                if let Some(set) = declared.prototype_over() {
                    prototype = Some(set);
                }
            }
            prototypes.push(prototype);
        }
        Some(prototypes.into_boxed_slice())
    }

    /// The row of `class`'s function table under `name`, with its
    /// declaration, its scope and the prototype the record holds for it.
    fn method_above(&self, class: Recorded<'a, H>, name: &[u8]) -> Option<Above<'a, H>> {
        let (at, row) = class.record.methods.find_at(name, |row| {
            self.row_member_name::<MethodDecl>(class.decl, row)
        })?;
        let (_, method) = self.row_member::<MethodDecl>(class.decl, row)?;
        Some(Above {
            method,
            scope: row.holder_in(class.handle),
            prototype: class.record.prototypes.get(at).copied().flatten(),
        })
    }

    /// The record's table of `M` as `zend_do_link_class` leaves it, in
    /// its order: the class's first `own` members, a trait's among them;
    /// each of the parent's table it does not declare and that passes
    /// on, `do_inherit_method`, `do_inherit_class_constant` and
    /// `do_inherit_property` (a private method or property among them,
    /// a private constant not); the rest of its own, which for a method
    /// are the traits' abstract ones it took; then each of its
    /// interfaces', in `zend_do_implement_interfaces`' order, that
    /// nothing above put there. An internal class's constants and
    /// properties are the parent's and the interfaces' and then its own
    /// ([`ClassMember::LAST_WHEN_INTERNAL`]), a property it redeclares
    /// moving to the end ([`ClassMember::REDECLARED_LAST`]). None where
    /// the parent or an interface is held with no record.
    ///
    /// `ce->properties_info` puts the class's own first, so it is not
    /// the slot order, which puts the parent's slots first.
    fn member_rows<M: ClassMember>(
        &self,
        child: &'a ClassDecl,
        own: usize,
    ) -> Option<Rows<Row<H>, M::Case>> {
        let named = |row: &Row<H>| self.row_member_name::<M>(child, row);
        let mut rows = Rows::default();
        let declared = (0u32..).zip(M::of(child));
        let last = M::LAST_WHEN_INTERNAL && child.file.0.is_empty();
        let own = if last { 0 } else { own };
        for (at, member) in declared.clone().take(own) {
            rows.add(Row { holder: None, at }, member.name(), named);
        }
        if let Some(parent) = &child.parent {
            self.inherit_rows::<M>(&mut rows, child, &parent.0)?;
        }
        if !last {
            for (at, member) in declared.clone().skip(own) {
                rows.add(Row { holder: None, at }, member.name(), named);
            }
        }
        let mut interfaces = Vec::new();
        self.interfaces_of(&child.interfaces, &mut interfaces, 0);
        for interface in interfaces {
            self.inherit_rows::<M>(&mut rows, child, &interface.name.0)?;
        }
        if last {
            for (at, member) in declared {
                let row = Row { holder: None, at };
                if M::REDECLARED_LAST {
                    rows.replace(row, member.name(), named);
                } else {
                    rows.add(row, member.name(), named);
                }
            }
        }
        Some(rows)
    }

    /// The rows of the record of the class `name` names into `child`'s,
    /// each that passes on and whose name `child`'s holds none of, its
    /// own rows naming it by its key. None where the class is held with
    /// no record; nothing where no class has the name.
    fn inherit_rows<M: ClassMember>(
        &self,
        rows: &mut Rows<Row<H>, M::Case>,
        child: &'a ClassDecl,
        name: &[u8],
    ) -> Option<()> {
        let Some(held) = (self.lookup)(name) else {
            return Some(());
        };
        let (handle, record) = held.record?;
        rows.inherit(
            M::table(record),
            |row| {
                let (_, member) = self.row_member::<M>(held.decl, row)?;
                member.inherited().then(|| Row {
                    holder: Some(row.holder_in(handle)),
                    at: row.at,
                })
            },
            |row| self.row_member_name::<M>(child, row),
            |row| self.row_member_name::<M>(held.decl, row),
        );
        Some(())
    }

    /// The name of the member a row of `class`'s record stands for.
    fn row_member_name<M: ClassMember>(&self, class: &'a ClassDecl, row: &Row<H>) -> &'a [u8] {
        self.row_member::<M>(class, row)
            .map_or(&[][..], |(_, member)| member.name())
    }

    /// The member a row of `class`'s record stands for, beside the class
    /// that declared it.
    fn row_member<M: ClassMember>(
        &self,
        class: &'a ClassDecl,
        row: &Row<H>,
    ) -> Option<(&'a ClassDecl, &'a M)> {
        let by = row.holder.map_or(Some(class), |h| (self.holder)(h))?;
        Some((by, M::of(by).get(usize::try_from(row.at).ok()?)?))
    }

    /// Every interface `class` implements, its own list and each one's
    /// parents, in the order `zend_do_implement_interfaces` meets them.
    fn interfaces_of(&self, names: &[Name], out: &mut Vec<&'a ClassDecl>, depth: u32) {
        for name in names {
            let Some(interface) = self.class_named(&name.0) else {
                continue;
            };
            if out.iter().any(|seen| same(seen, interface)) || depth > 64 {
                continue;
            }
            out.push(interface);
            self.interfaces_of(&interface.interfaces, out, depth + 1);
        }
    }

    /// `ce->interfaces` as `zend_do_link_class` leaves it, each once: a
    /// user class naming some keeps its parent's, adds them, then each
    /// one's own reversed (`zend_do_implement_interfaces`); one naming
    /// none takes its parent's reversed (`zend_do_inherit_interfaces`).
    /// An internal class takes `Stringable` for a `__toString`, its
    /// parent's reversed, then each it names with that one's own
    /// reversed after it (`zend_class_implements`). None where the
    /// parent or an interface is held with no record.
    fn interface_list(&self, child: &'a ClassDecl) -> Option<Box<[H]>>
    where
        H: PartialEq,
    {
        let record =
            |name: &[u8]| (self.lookup)(name).map_or(Some(None), |held| held.record.map(Some));
        let mut named = Vec::new();
        for name in &child.interfaces {
            named.extend(record(&name.0)?);
        }
        let parent = match &child.parent {
            Some(name) => record(&name.0)?,
            None => None,
        };
        let inherited = parent.map_or(&[][..], |(_, parent)| &*parent.interfaces);
        let internal = child.file.0.is_empty();
        let mut list = Vec::new();
        if names_stringable(child)
            && let Some((key, _)) = record(b"Stringable")?
        {
            list.push(key);
        }
        if internal || named.is_empty() {
            extend_new(&mut list, inherited.iter().rev());
        } else {
            list.extend_from_slice(inherited);
        }
        let mut added = Vec::new();
        for (key, interface) in named {
            if !list.contains(&key) {
                list.push(key);
                added.push(interface);
            }
            if internal {
                for interface in added.drain(..) {
                    extend_new(&mut list, interface.interfaces.iter().rev());
                }
            }
        }
        for interface in added {
            extend_new(&mut list, interface.interfaces.iter().rev());
        }
        Some(list.into_boxed_slice())
    }

    /// `lookup_class(scope, name)` while `child` links: the class being
    /// linked by its name where it stands in the table, and while the
    /// file compiles, where it does not yet, only from its own scope,
    /// `lookup_class_ex`'s `zend_string_equals_ci(scope->name, name)`
    /// ([`Standing`]); else the class the table holds.
    fn lookup(&self, child: &'a ClassDecl, scope: &Scope<'a>, name: &[u8]) -> Option<Cls<'a>> {
        if child.name.0.eq_ignore_ascii_case(name)
            && (self.standing == Standing::Declared || scope.class == Cls::Linking)
        {
            return Some(Cls::Linking);
        }
        self.class_named(name).map(Cls::Linked)
    }

    /// `unlinked_instanceof`.
    fn subclass(&self, child: &'a ClassDecl, a: Cls<'a>, b: Cls<'a>) -> bool {
        match (a, b) {
            (Cls::Linking, Cls::Linking) => true,
            (Cls::Linked(x), Cls::Linked(y)) => self.instance_of(x, y),
            (Cls::Linked(x), Cls::Linking) => self.instance_of(x, child),
            (Cls::Linking, Cls::Linked(y)) => child
                .parent
                .iter()
                .chain(child.interfaces.iter())
                .filter_map(|name| self.class_named(&name.0))
                .any(|up| self.instance_of(up, y)),
        }
    }

    fn scope_of(child: &'a ClassDecl, class: Cls<'a>) -> Scope<'a> {
        let decl = match class {
            Cls::Linking => child,
            Cls::Linked(decl) => decl,
        };
        Scope {
            name: &decl.name.0,
            parent: decl.parent.as_ref().map(|p| &*p.0),
            class,
            is_final: decl.flags.is_final,
        }
    }

    /// `zend_type_permits_self`.
    fn permits_self(&self, child: &'a ClassDecl, proto: &Shape, fe_scope: &Scope<'a>) -> bool {
        if proto.mask & MAY_BE_OBJECT != 0 {
            return true;
        }
        proto.members.iter().any(|member| match member {
            Member::Name(name) => self
                .lookup(child, fe_scope, name)
                .is_some_and(|ce| self.subclass(child, fe_scope.class, ce)),
            Member::Intersection(_) => false,
        })
    }

    /// `zend_is_class_subtype_of_type`.
    fn class_subtype_of_type(
        &self,
        child: &'a ClassDecl,
        fe_scope: &Scope<'a>,
        fe_name: &[u8],
        proto_scope: &Scope<'a>,
        proto: &Shape,
    ) -> Status {
        let mut unresolved = false;
        let fe_ce = self.lookup(child, fe_scope, fe_name);
        if proto.mask & MAY_BE_OBJECT != 0 {
            if fe_ce.is_some() {
                return Status::Success;
            }
            unresolved = true;
        }
        if proto.mask & MAY_BE_CALLABLE != 0 {
            match fe_ce {
                None => unresolved = true,
                Some(_) if fe_name.eq_ignore_ascii_case(b"Closure") => return Status::Success,
                Some(_) => {}
            }
        }
        if proto.mask & MAY_BE_STATIC != 0 && fe_scope.is_final {
            match fe_ce {
                None => unresolved = true,
                Some(ce) if ce == fe_scope.class => return Status::Success,
                Some(_) => {}
            }
        }
        let is_intersection = proto.intersection;
        for member in &proto.members {
            match member {
                Member::Intersection(names) => {
                    let inner = Shape {
                        intersection: true,
                        members: names.iter().cloned().map(Member::Name).collect(),
                        mask: 0,
                    };
                    match self.class_subtype_of_type(child, fe_scope, fe_name, proto_scope, &inner)
                    {
                        Status::Error if is_intersection => return Status::Error,
                        Status::Unresolved => unresolved = true,
                        Status::Success if !is_intersection => return Status::Success,
                        _ => {}
                    }
                }
                Member::Name(proto_name) => {
                    if fe_name.eq_ignore_ascii_case(proto_name) {
                        if !is_intersection {
                            return Status::Success;
                        }
                        continue;
                    }
                    let (Some(fe_ce), Some(proto_ce)) =
                        (fe_ce, self.lookup(child, proto_scope, proto_name))
                    else {
                        unresolved = true;
                        continue;
                    };
                    if self.subclass(child, fe_ce, proto_ce) {
                        if !is_intersection {
                            return Status::Success;
                        }
                    } else if is_intersection {
                        return Status::Error;
                    }
                }
            }
        }
        if unresolved {
            Status::Unresolved
        } else if is_intersection {
            Status::Success
        } else {
            Status::Error
        }
    }

    /// `zend_is_intersection_subtype_of_class`.
    fn intersection_subtype_of_class(
        &self,
        child: &'a ClassDecl,
        fe_scope: &Scope<'a>,
        fe_names: &[Vec<u8>],
        proto_scope: &Scope<'a>,
        proto_name: &[u8],
    ) -> Status {
        let mut unresolved = false;
        for fe_name in fe_names {
            if fe_name.eq_ignore_ascii_case(proto_name) {
                return Status::Success;
            }
            let (Some(fe_ce), Some(proto_ce)) = (
                self.lookup(child, fe_scope, fe_name),
                self.lookup(child, proto_scope, proto_name),
            ) else {
                unresolved = true;
                continue;
            };
            if self.subclass(child, fe_ce, proto_ce) {
                return Status::Success;
            }
        }
        if unresolved {
            Status::Unresolved
        } else {
            Status::Error
        }
    }

    /// `zend_is_intersection_subtype_of_type`.
    fn intersection_subtype_of_type(
        &self,
        child: &'a ClassDecl,
        fe_scope: &Scope<'a>,
        fe_names: &[Vec<u8>],
        proto_scope: &Scope<'a>,
        proto: &Shape,
    ) -> Status {
        let mut unresolved = false;
        if proto.mask & MAY_BE_OBJECT != 0 {
            for fe_name in fe_names {
                if self.lookup(child, fe_scope, fe_name).is_some() {
                    return Status::Success;
                }
                unresolved = true;
            }
        }
        let early = if proto.intersection {
            Status::Error
        } else {
            Status::Success
        };
        for member in &proto.members {
            let status = match member {
                Member::Intersection(names) => {
                    let inner = Shape {
                        intersection: true,
                        members: names.iter().cloned().map(Member::Name).collect(),
                        mask: 0,
                    };
                    self.intersection_subtype_of_type(
                        child,
                        fe_scope,
                        fe_names,
                        proto_scope,
                        &inner,
                    )
                }
                Member::Name(name) => {
                    self.intersection_subtype_of_class(child, fe_scope, fe_names, proto_scope, name)
                }
            };
            if status == early {
                return status;
            }
            if status == Status::Unresolved {
                unresolved = true;
            }
        }
        if unresolved {
            Status::Unresolved
        } else if early == Status::Error {
            Status::Success
        } else {
            Status::Error
        }
    }

    /// `zend_perform_covariant_type_check`: whether `fe` in `fe_scope`
    /// is a subtype of `proto` in `proto_scope`, each class either
    /// names that no table holds registered where it cannot say.
    fn covariant(
        &self,
        child: &'a ClassDecl,
        fe_scope: &Scope<'a>,
        fe: &Type,
        proto_scope: &Scope<'a>,
        proto: &Type,
    ) -> Status {
        let status = self.covariant_status(child, fe_scope, fe, proto_scope, proto);
        if status == Status::Unresolved {
            self.register(child, fe, fe_scope);
            self.register(child, proto, proto_scope);
        }
        status
    }

    fn covariant_status(
        &self,
        child: &'a ClassDecl,
        fe_scope: &Scope<'a>,
        fe: &Type,
        proto_scope: &Scope<'a>,
        proto: &Type,
    ) -> Status {
        let fe = shape(fe, fe_scope);
        let proto = shape(proto, proto_scope);
        if proto.mask == Type::MAY_BE_ANY && fe.mask & MAY_BE_VOID == 0 {
            return Status::Success;
        }
        let mut added = fe.mask & !proto.mask;
        if added != 0 {
            if added & MAY_BE_STATIC != 0 && self.permits_self(child, &proto, fe_scope) {
                added &= !MAY_BE_STATIC;
            }
            if added == MAY_BE_NEVER {
                return Status::Success;
            }
            if added != 0 {
                return Status::Error;
            }
        }
        let mut unresolved = false;
        if fe.intersection {
            let early = if proto.intersection {
                Status::Error
            } else {
                Status::Success
            };
            let names: Vec<Vec<u8>> = fe
                .members
                .iter()
                .filter_map(|m| match m {
                    Member::Name(name) => Some(name.clone()),
                    Member::Intersection(_) => None,
                })
                .collect();
            let status =
                self.intersection_subtype_of_type(child, fe_scope, &names, proto_scope, &proto);
            if status == early {
                return status;
            }
            if status == Status::Unresolved {
                unresolved = true;
            }
            if !unresolved {
                return if early == Status::Error {
                    Status::Success
                } else {
                    Status::Error
                };
            }
            return Status::Unresolved;
        }
        for member in &fe.members {
            let status = match member {
                Member::Intersection(names) => {
                    self.intersection_subtype_of_type(child, fe_scope, names, proto_scope, &proto)
                }
                Member::Name(name) => {
                    self.class_subtype_of_type(child, fe_scope, name, proto_scope, &proto)
                }
            };
            if status == Status::Error {
                return Status::Error;
            }
            if status == Status::Unresolved {
                unresolved = true;
            }
        }
        if unresolved {
            Status::Unresolved
        } else {
            Status::Success
        }
    }

    /// `zend_do_perform_implementation_check`.
    fn implementation(
        &self,
        child: &'a ClassDecl,
        fe: &MethodDecl,
        fe_scope: &Scope<'a>,
        proto: &MethodDecl,
        proto_scope: &Scope<'a>,
    ) -> Status {
        let (f, p) = (&fe.function, &proto.function);
        if required(p) < required(f) {
            return Status::Error;
        }
        if p.returns_by_ref && !f.returns_by_ref {
            return Status::Error;
        }
        let (proto_variadic, fe_variadic) = (is_variadic(p), is_variadic(f));
        if proto_variadic && !fe_variadic {
            return Status::Error;
        }
        let (proto_count, fe_count) = (p.parameters.len(), f.parameters.len());
        let mut status = Status::Success;
        for at in 0..proto_count.max(fe_count) {
            let proto_arg = p
                .parameters
                .get(at)
                .or_else(|| proto_variadic.then(|| p.parameters.last()).flatten());
            let fe_arg = f
                .parameters
                .get(at)
                .or_else(|| fe_variadic.then(|| f.parameters.last()).flatten());
            let Some(proto_arg) = proto_arg else {
                continue;
            };
            let Some(fe_arg) = fe_arg else {
                return Status::Error;
            };
            let local = match (&fe_arg.ty, &proto_arg.ty) {
                (None, _) => Status::Success,
                (Some(ty), _) if ty.mask() == Type::MAY_BE_ANY => Status::Success,
                (Some(_), None) => Status::Error,
                (Some(fe_ty), Some(proto_ty)) => {
                    self.covariant(child, proto_scope, proto_ty, fe_scope, fe_ty)
                }
            };
            match local {
                Status::Error => return Status::Error,
                Status::Unresolved => status = Status::Unresolved,
                _ => {}
            }
            if fe_arg.by_ref != proto_arg.by_ref {
                return Status::Error;
            }
        }
        if let Some(proto_ret) = &p.return_type {
            let Some(fe_ret) = &f.return_type else {
                if !proto.tentative_return {
                    return Status::Error;
                }
                return if status == Status::Success {
                    Status::Warning
                } else {
                    status
                };
            };
            let local = self.covariant(child, fe_scope, fe_ret, proto_scope, proto_ret);
            if local != Status::Success {
                if local == Status::Error && proto.tentative_return {
                    return Status::Warning;
                }
                return local;
            }
        }
        status
    }

    /// The two sides of a check of the child's own method against one a
    /// parent or an interface declared.
    fn own_over(child: &'a ClassDecl, proto_class: &'a ClassDecl) -> (Side<'a>, Side<'a>) {
        (
            Side {
                scope: Self::scope_of(child, Cls::Linking),
                shown: &child.name.0,
            },
            Side {
                scope: Self::scope_of(child, Cls::Linked(proto_class)),
                shown: &proto_class.name.0,
            },
        )
    }

    /// `do_inheritance_check_on_method` of `fe` over `proto`, each read
    /// on its side, with the checks `check` names.
    ///
    /// ```php
    /// class P { function f() {} }
    /// abstract class C extends P { abstract function f(); }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot make non abstract method P::f() abstract in class C in %s on line 3
    /// ```
    ///
    /// An override of a native's tentative return type that does not
    /// match it is php's deprecation, and `#[\ReturnTypeWillChange]` on
    /// the override silences it:
    ///
    /// ```php
    /// class C implements IteratorAggregate { #[\ReturnTypeWillChange] function getIterator() { return new ArrayIterator([1]); } }
    /// class D implements IteratorAggregate { function getIterator() { return new ArrayIterator([2]); } }
    /// foreach (new C as $v) echo $v, "\n";
    /// ```
    /// ```expect
    /// Deprecated: Return type of D::getIterator() should either be compatible with IteratorAggregate::getIterator(): Traversable, or the #[\ReturnTypeWillChange] attribute should be used to temporarily suppress the notice in %s on line 3
    /// 1
    /// ```
    fn method_refused(
        &mut self,
        child: &'a ClassDecl,
        fe: &MethodDecl,
        fe_side: Side<'a>,
        proto: &MethodDecl,
        proto_side: Side<'a>,
        check: Check,
    ) -> Option<Linked> {
        let (fe_scope, proto_scope) = (fe_side.scope, proto_side.scope);
        let interface = check == Check::Interface;
        let site = Site {
            file: fe.function.file.clone(),
            line: fe.function.line,
        };
        let refused = |refusal: Refusal| {
            Some(Linked::Refused(Refused {
                refusal,
                site: site.clone(),
            }))
        };
        let parent = text(proto_side.shown);
        let method = text(&fe.function.name.0);
        let class = text(fe_side.shown);
        let is_ctor = proto.function.name.0.eq_ignore_ascii_case(b"__construct");
        if proto.visibility == Visibility::Private && !proto.is_abstract && !is_ctor {
            return None;
        }
        if proto.is_final {
            return refused(Refusal::FinalMethod { parent, method });
        }
        if fe.is_instance != proto.is_instance {
            return refused(Refusal::MethodReceiver {
                parent,
                method,
                class,
                parent_static: !proto.is_instance,
            });
        }
        if fe.is_abstract && !proto.is_abstract {
            return refused(Refusal::AbstractMethod {
                parent,
                method,
                class,
            });
        }
        if is_ctor && !proto.is_abstract && !interface {
            return None;
        }
        if check != Check::TraitAbstract && strength(fe.visibility) > strength(proto.visibility) {
            return refused(Refusal::MethodVisibility {
                class,
                method,
                visibility: proto.visibility,
                parent,
            });
        }
        self.deferred.borrow_mut().missing.clear();
        let status = self.implementation(child, fe, &fe_scope, proto, &proto_scope);
        let child_text = declaration(&fe.function, &fe_side);
        let parent_text = declaration(&proto.function, &proto_side);
        match status {
            Status::Success => None,
            Status::Warning => {
                let silenced = fe
                    .function
                    .attributes
                    .iter()
                    .any(|a| a.name.0.eq_ignore_ascii_case(b"ReturnTypeWillChange"));
                if !silenced {
                    self.raised.push(Raised::TentativeReturn(TentativeReturn {
                        child: child_text,
                        parent: parent_text,
                        site,
                    }));
                }
                None
            }
            Status::Error => refused(Refusal::Incompatible {
                child: child_text,
                parent: parent_text,
            }),
            Status::Unresolved => {
                let missing = self.deferred.borrow().missing.first().cloned();
                let refusal = match missing {
                    Some(missing) => Refusal::Unavailable {
                        child: child_text,
                        parent: parent_text,
                        missing: text(&missing),
                    },
                    None => Refusal::Incompatible {
                        child: child_text,
                        parent: parent_text,
                    },
                };
                self.defer(Refused { refusal, site })
            }
        }
    }

    /// `do_inherit_property` for a property the child redeclares, in
    /// php's order: the flags ([`Self::property_refused`]), then each
    /// hook ([`Self::hooks_refused`]), then the type
    /// ([`Self::hooked_type_refused`]), a type check that needed a
    /// class no table holds being the obligation php leaves. An
    /// interface's property is held to it as a parent's is
    /// (`do_interface_implementation`).
    ///
    /// ```php
    /// interface I { public int $x { get; } }
    /// class C implements I { protected int $x = 1; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Access level to C::$x must be public (as in class I) in %s on line 3
    /// ```
    fn inherited_property_refused(
        &mut self,
        child: &'a ClassDecl,
        own: &'a PropertyDecl,
        parent: &'a PropertyDecl,
        parent_class: &'a ClassDecl,
        check: Check,
    ) -> Option<Linked> {
        if let Some(refusal) = Self::property_refused(child, own, parent, parent_class) {
            return Some(Linked::Refused(at_class(child, refusal)));
        }
        if parent.visibility == Visibility::Private {
            return None;
        }
        if let Some(linked) = self.hooks_refused(child, own, parent, parent_class, check) {
            return Some(linked);
        }
        let (refusal, status) = self.hooked_type_refused(child, own, parent, parent_class)?;
        let refused = at_class(child, refusal);
        if status == Status::Unresolved {
            return self.defer(refused);
        }
        Some(Linked::Refused(refused))
    }

    /// The flags half of `do_inherit_property` for a property the child
    /// redeclares: `final`, `static`, `readonly` unless the parent is
    /// abstract, the set visibility unless the parent is a virtual
    /// property with no `set` hook, and the visibility.
    fn property_refused(
        child: &'a ClassDecl,
        own: &PropertyDecl,
        parent: &PropertyDecl,
        parent_class: &'a ClassDecl,
    ) -> Option<Refusal> {
        let parent_name = text(&parent_class.name.0);
        let class = text(&child.name.0);
        let property = text(&own.name.0);
        if parent.is_final {
            return Some(Refusal::FinalProperty {
                parent: parent_name,
                property,
            });
        }
        if parent.visibility == Visibility::Private {
            return None;
        }
        let refusal = if parent.is_instance != own.is_instance {
            Refusal::PropertyReceiver {
                parent: parent_name,
                class,
                property,
                parent_static: !parent.is_instance,
            }
        } else if parent.is_readonly != own.is_readonly
            && !parent.hooks.as_ref().is_some_and(|hooks| hooks.is_abstract)
        {
            Refusal::ReadonlyProperty {
                parent: parent_name,
                class,
                property,
                parent_readonly: parent.is_readonly,
            }
        } else if Self::set_visibility_binds(parent)
            && own.set_visibility.is_some_and(|set| {
                strength(set) > strength(parent.set_visibility.unwrap_or(parent.visibility))
            })
        {
            Refusal::SetVisibility {
                class,
                property,
                parent: parent_name,
                parent_set: parent.set_visibility,
            }
        } else if strength(own.visibility) > strength(parent.visibility) {
            Refusal::PropertyVisibility {
                class,
                property,
                parent: parent_name,
                visibility: parent.visibility,
            }
        } else {
            return None;
        };
        Some(refusal)
    }

    /// `do_inherit_constant_check` for a constant the child's table
    /// already holds, from `held_by`, none for the child's own: the
    /// refusals of [`Self::constant_flags_refused`], then a typed
    /// parent's `class_constant_types_compatible`, php's covariant check
    /// of the held constant's type in its class against the parent's in
    /// the parent's, an unresolved outcome the obligation php delays.
    ///
    /// ```php
    /// class P { const int X = 1; const ?int Y = 1; }
    /// class C extends P { const int Y = 2; }
    /// var_dump(C::X, C::Y);
    /// class D extends P { const string X = 'a'; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type of D::X must be compatible with P::X of type int in %s on line 5
    /// ```
    ///
    /// ```php
    /// interface I { const int X = 1; }
    /// class C implements I { const X = 2; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type of C::X must be compatible with I::X of type int in %s on line 3
    /// ```
    fn constant_refused(
        &self,
        child: &'a ClassDecl,
        held: &ClassConstDecl,
        held_by: Option<&'a ClassDecl>,
        parent: &ClassConstDecl,
        parent_class: &'a ClassDecl,
    ) -> Option<Linked> {
        if let Some(refusal) =
            Self::constant_flags_refused(child, held, held_by, parent, parent_class)
        {
            return Some(Linked::Refused(at_class(child, refusal)));
        }
        let parent_ty = parent.ty.as_ref()?;
        if parent.visibility == Visibility::Private {
            return None;
        }
        let holder = held_by.unwrap_or(child);
        let fe_scope = Self::scope_of(child, held_by.map_or(Cls::Linking, Cls::Linked));
        let proto_scope = Self::scope_of(child, Cls::Linked(parent_class));
        let status = match &held.ty {
            None => Status::Error,
            Some(ty) => self.covariant(child, &fe_scope, ty, &proto_scope, parent_ty),
        };
        let refused = at_class(
            child,
            Refusal::ConstantType {
                holder: text(&holder.name.0),
                constant: text(&parent.name.0),
                parent: text(&parent_class.name.0),
                ty: type_to_string(&resolved(parent_ty, &proto_scope)),
            },
        );
        match status {
            Status::Error => Some(Linked::Refused(refused)),
            Status::Unresolved => self.defer(refused),
            Status::Success | Status::Warning => None,
        }
    }

    /// The modifier half of `do_inherit_constant_check`: a final constant
    /// overridden, one inherited from two places, and a narrowed
    /// visibility.
    fn constant_flags_refused(
        child: &'a ClassDecl,
        held: &ClassConstDecl,
        held_by: Option<&'a ClassDecl>,
        parent: &ClassConstDecl,
        parent_class: &'a ClassDecl,
    ) -> Option<Refusal> {
        let parent_name = text(&parent_class.name.0);
        let holder = held_by.map_or_else(|| text(&child.name.0), |decl| text(&decl.name.0));
        let constant = text(&parent.name.0);
        let held_by_parent = held_by.is_some_and(|by| same(by, parent_class));
        if !held_by_parent && parent.is_final {
            return Some(Refusal::FinalConstant {
                holder,
                parent: parent_name,
                constant,
            });
        }
        if held_by.is_some() && !held_by_parent {
            return Some(Refusal::AmbiguousConstant {
                kind: child.kind,
                class: text(&child.name.0),
                holder,
                parent: parent_name,
                constant,
            });
        }
        (strength(held.visibility) > strength(parent.visibility)).then(|| {
            Refusal::ConstantVisibility {
                class: text(&child.name.0),
                constant,
                visibility: parent.visibility,
                parent_kind: parent_class.kind,
                parent: parent_name,
            }
        })
    }

    /// `zend_do_inheritance_ex`'s refusals of the parent itself.
    ///
    /// ```php
    /// final class F {}
    /// class G extends F {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class G cannot extend final class F in %s on line 3
    /// ```
    ///
    /// ```php
    /// readonly class R {}
    /// class S extends R {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Non-readonly class S cannot extend readonly class R in %s on line 3
    /// ```
    ///
    /// ```php
    /// class N {}
    /// readonly class S extends N {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Readonly class S cannot extend non-readonly class N in %s on line 3
    /// ```
    ///
    /// ```php
    /// abstract class A {}
    /// readonly class R {}
    /// readonly class S extends R {}
    /// class B extends A {}
    /// echo get_class(new B), " ", get_class(new S), "\n";
    /// ```
    /// ```expect
    /// B S
    /// ```
    fn parent_refused(child: &ClassDecl, parent: &ClassDecl) -> Option<Refusal> {
        let (class, parent_name) = (text(&child.name.0), text(&parent.name.0));
        if child.kind == ClassKind::Interface {
            return (parent.kind != ClassKind::Interface).then_some(
                Refusal::InterfaceExtendsClass {
                    interface: class,
                    class: parent_name,
                },
            );
        }
        if parent.kind == ClassKind::Enum {
            return Some(Refusal::ExtendsEnum {
                class,
                parent: parent_name,
            });
        }
        if parent.flags.is_final {
            return Some(Refusal::ExtendsFinal {
                class,
                parent: parent_name,
            });
        }
        if matches!(parent.kind, ClassKind::Interface | ClassKind::Trait) {
            return Some(Refusal::ExtendsNotAClass {
                class,
                kind: parent.kind,
                parent: parent_name,
            });
        }
        (child.flags.is_readonly != parent.flags.is_readonly).then_some(Refusal::ReadonlyClass {
            class,
            readonly: child.flags.is_readonly,
            parent: parent_name,
        })
    }

    /// Everything `zend_do_link_class` refuses about `child`, the first
    /// refusal in php's order, a class that uses traits already
    /// composed with them and `abstracts` the copies of their abstract
    /// methods, which php binds after the parent.
    fn link_refused(&mut self, child: &'a ClassDecl, abstracts: &'a [traits::Copy]) -> Settled {
        // What the child's table holds as the checks go, each with the
        // class that put it there, none for the child's own.
        let mut constants: Constants<'a> = Rows::default();
        for constant in &child.constants {
            put(&mut constants, None, constant);
        }
        let mut methods: Methods<'a> = Rows::default();
        for method in &child.methods {
            put(&mut methods, None, method);
        }
        if let Some(parent_name) = &child.parent
            && let Some(parent) = self.class_named(&parent_name.0)
        {
            self.depend(parent_name);
            if let Some(linked) = self.inherit(child, parent, &mut constants, &mut methods) {
                return Settled::of(linked);
            }
        }
        let added = match self.trait_abstracts(child, abstracts, &mut methods) {
            Ok(added) => added,
            Err(linked) => return Settled::of(*linked),
        };
        if let Some(linked) = self.implement(child, &mut constants, &mut methods) {
            return Settled::of(linked);
        }
        self.serializable_deprecated(child, &methods);
        let hooks = self.abstract_hooks(child);
        if let Some(refusal) = abstract_left(child, methods.rows(), hooks) {
            return Settled::of(Linked::Refused(at_class(child, refusal)));
        }
        if let Some(refusal) = enum_properties(child).or_else(|| self.enum_refused(child)) {
            return Settled::of(Linked::Refused(at_class(child, refusal)));
        }
        if let Some(linked) = self.set_hooks_refused(child) {
            return Settled::of(linked);
        }
        let stringable = match self.stringable(child, &methods) {
            Ok(stringable) => stringable,
            Err(linked) => return Settled::of(*linked),
        };
        if let Some(refused) = self.override_refused(child, abstracts) {
            return Settled::of(Linked::Refused(refused));
        }
        Settled {
            outcome: Linked::Linked,
            added,
            stringable,
        }
    }

    /// `zend_inheritance_check_override`, once every other check held:
    /// a method or a property of the class's own marked `#[\Override]`
    /// that overrides nothing. A method overrides what
    /// `do_inheritance_check_on_method` resets the mark over: a
    /// parent's method that is abstract or, not being a constructor, is
    /// not private; an interface's, the class's own or its parents'; and
    /// a used trait's abstract one. A property overrides a parent's that
    /// is not private. The method's refusal stands at its first line,
    /// the property's at the class's.
    ///
    /// ```php
    /// interface I { function i(); }
    /// abstract class P { abstract function a(); function b() {} private function c() {} public $x; }
    /// class C extends P implements I {
    ///     #[\Override] function i() {}
    ///     #[\Override] function a() {}
    ///     #[\Override] function b() {}
    ///     #[\Override] public $x;
    /// }
    /// echo "linked\n";
    /// class D extends P { function a() {}
    ///     #[\Override] private function c() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: D::c() has #[\Override] attribute, but no matching parent method exists in %s on line 12
    /// ```
    ///
    /// ```php
    /// class P { function __construct() {} }
    /// class C extends P { #[\Override] function __construct() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: C::__construct() has #[\Override] attribute, but no matching parent method exists in %s on line 3
    /// ```
    ///
    /// ```php
    /// class C { #[\Override] public $x; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: C::$x has #[\Override] attribute, but no matching parent property exists in %s on line 2
    /// ```
    ///
    /// ```php
    /// interface I { public mixed $i { get; } }
    /// interface II extends I { #[\Override] public mixed $i { get; } }
    /// class C implements II { #[\Override] public mixed $i; }
    /// echo "Done";
    /// ```
    /// ```expect
    /// Done
    /// ```
    fn override_refused(
        &self,
        child: &'a ClassDecl,
        abstracts: &[traits::Copy],
    ) -> Option<Refused> {
        if child.kind == ClassKind::Trait {
            return None;
        }
        let marked = |attributes: &[php_unit::decl::Attribute]| {
            attributes
                .iter()
                .any(|a| a.name.0.eq_ignore_ascii_case(b"Override"))
        };
        let parent = self.class_parent(child);
        let inherited = parent.map(|parent| self.members_above::<PropertyDecl>(parent));
        let mut names = child.interfaces.clone();
        let mut above = parent;
        let mut depth = 0;
        while let Some(class) = above
            && depth < 64
        {
            names.extend(class.interfaces.iter().cloned());
            above = self.class_parent(class);
            depth += 1;
        }
        let mut interfaces = Vec::new();
        self.interfaces_of(&names, &mut interfaces, 0);
        let parent_methods = parent
            .filter(|_| child.methods.iter().any(|m| marked(&m.function.attributes)))
            .map(|parent| self.methods_above(parent))
            .unwrap_or_default();
        for method in &child.methods {
            if !marked(&method.function.attributes) {
                continue;
            }
            let is_ctor = method.function.name.0.eq_ignore_ascii_case(b"__construct");
            let by_parent = parent_methods.iter().any(|(_, p)| {
                same_method(p, method)
                    && (p.is_abstract || (!is_ctor && p.visibility != Visibility::Private))
            });
            let by_interface = interfaces
                .iter()
                .any(|interface| interface.methods.iter().any(|m| same_method(m, method)));
            let by_trait = abstracts
                .iter()
                .any(|copy| method.function.name.0.eq_ignore_ascii_case(&copy.key));
            if !(by_parent || by_interface || by_trait) {
                return Some(Refused {
                    refusal: Refusal::OverrideMethod {
                        class: text(&child.name.0),
                        method: text(&method.function.name.0),
                    },
                    site: Site {
                        file: method.function.file.clone(),
                        line: method.function.line,
                    },
                });
            }
        }
        for property in &child.properties {
            if !marked(&property.attributes) {
                continue;
            }
            let by_parent = inherited.as_ref().is_some_and(|table| {
                table
                    .find(&property.name.0, member_name)
                    .is_some_and(|(_, p)| p.visibility != Visibility::Private)
            });
            let by_interface = interfaces
                .iter()
                .any(|interface| interface.properties.iter().any(|p| p.name == property.name));
            if !(by_parent || by_interface) {
                return Some(at_class(
                    child,
                    Refusal::OverrideProperty {
                        class: text(&child.name.0),
                        property: text(&property.name.0),
                    },
                ));
            }
        }
        self.hook_override_refused(child, parent, &interfaces)
    }

    /// `zend_inheritance_check_override`'s hook arm: a hook of the
    /// class's own marked `#[\Override]` overrides what the parent's or
    /// an interface's property of its name does of its kind,
    /// `property_has_operation`: a backed property reads, and writes
    /// unless it is readonly, and a hooked one has the hook.
    ///
    /// ```php
    /// class A { public $prop { set {} } }
    /// class B extends A { public $prop { #[Override] get => 1; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: B::$prop::get() has #[\Override] attribute, but no matching parent method exists in %s on line 3
    /// ```
    fn hook_override_refused(
        &self,
        child: &'a ClassDecl,
        parent: Option<&'a ClassDecl>,
        interfaces: &[&'a ClassDecl],
    ) -> Option<Refused> {
        let lookup = |name: &[u8]| (self.lookup)(name).map(Held::plain);
        for property in &child.properties {
            let Some(hooks) = &property.hooks else {
                continue;
            };
            for kind in HookKind::ALL {
                let Some(hook) = hooks.of(kind) else {
                    continue;
                };
                if !hook
                    .function
                    .attributes
                    .iter()
                    .any(|a| a.name.0.eq_ignore_ascii_case(b"Override"))
                {
                    continue;
                }
                let by_parent = parent
                    .and_then(|parent| resolve_property(parent, &property.name.0, &lookup))
                    .filter(|resolved| resolved.property.visibility != Visibility::Private)
                    .is_some_and(|resolved| {
                        (!resolved.is_virtual
                            && (kind == HookKind::Get || !resolved.property.is_readonly))
                            || resolved.hook(kind).is_some()
                    });
                let by_interface = interfaces.iter().any(|interface| {
                    interface.properties.iter().any(|p| {
                        p.name == property.name
                            && p.hooks.as_ref().is_some_and(|h| h.of(kind).is_some())
                    })
                });
                if !(by_parent || by_interface) {
                    return Some(Refused {
                        refusal: Refusal::OverrideMethod {
                            class: text(&child.name.0),
                            method: text(&hook.function.name.0),
                        },
                        site: Site {
                            file: hook.function.file.clone(),
                            line: hook.function.line,
                        },
                    });
                }
            }
        }
        None
    }

    /// The second pass of `zend_do_traits_method_binding`, made once the
    /// parent is inherited: each abstract method of a used trait, in
    /// php's order, is held against the method the class has under its
    /// name, its own, a trait's or its parent's, as
    /// `zend_add_trait_method` holds it, and taken as the class's own
    /// where it has none. A copy the pass took itself still has its
    /// trait's scope, so the same body at the same visibility again is
    /// no second method. The answer is the copies taken, by index.
    ///
    /// ```php
    /// trait T { abstract function f(); abstract public function g(int $a): int; }
    /// class P { function f() { return "P"; } }
    /// class C extends P { use T; function g(int $a): int { return $a; } }
    /// echo (new C)->f(), (new C)->g(1), "\n";
    /// ```
    /// ```expect exit=0
    /// P1
    /// ```
    ///
    /// ```php
    /// trait T { abstract private function f(int $a); }
    /// class P { private function f(string $a) {} }
    /// class C extends P { use T; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Declaration of P::f(string $a) must be compatible with T::f(int $a) in %s on line 3
    /// ```
    ///
    /// ```php
    /// trait A { abstract static function f(); }
    /// class C { use A; function f() {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot make static method A::f() non static in class C in %s on line 3
    /// ```
    ///
    /// ```php
    /// trait A { abstract function f(); }
    /// class C { use A; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C contains 1 abstract method and must therefore be declared abstract or implement the remaining method (C::f) in %s on line 3
    /// ```
    fn trait_abstracts(
        &mut self,
        child: &'a ClassDecl,
        copies: &'a [traits::Copy],
        methods: &mut Methods<'a>,
    ) -> Result<Vec<usize>, Box<Linked>> {
        let mut added: Vec<usize> = Vec::new();
        for (at, copy) in copies.iter().enumerate() {
            let Some(&(holder, existing)) = methods.find(&copy.key, member_name) else {
                methods.add((None, &copy.method), &copy.key, member_name);
                added.push(at);
                continue;
            };
            let taken = added
                .iter()
                .map(|a| &copies[*a])
                .find(|taken| std::ptr::eq(&raw const taken.method, existing));
            if let Some(taken) = taken
                && taken.body == copy.body
                && taken.method.visibility == copy.method.visibility
            {
                continue;
            }
            let fe_side = match (taken, holder) {
                (Some(taken), _) => Side {
                    scope: Self::scope_of(child, Cls::Linking),
                    shown: &taken.from.0,
                },
                (None, None) => Side {
                    scope: Self::scope_of(child, Cls::Linking),
                    shown: &child.name.0,
                },
                (None, Some(declaring)) => Side {
                    scope: Self::scope_of(child, Cls::Linked(declaring)),
                    shown: &declaring.name.0,
                },
            };
            let proto_side = Side {
                scope: Self::scope_of(child, Cls::Linking),
                shown: &copy.from.0,
            };
            if let Some(linked) = self.method_refused(
                child,
                existing,
                fe_side,
                &copy.method,
                proto_side,
                Check::TraitAbstract,
            ) {
                return Err(Box::new(linked));
            }
        }
        Ok(added)
    }

    /// The end of `zend_do_link_class`: a class a trait gave
    /// `__toString` implements `Stringable`, which the compiler adds for
    /// a class that declares it, and is held to it as
    /// `do_interface_implementation` holds any class.
    ///
    /// ```php
    /// trait T { function __toString(): string { return "s"; } }
    /// class C { use T; }
    /// var_dump(new C instanceof Stringable, (string) new C);
    /// ```
    /// ```expect exit=0
    /// bool(true)
    /// string(1) "s"
    /// ```
    fn stringable(
        &mut self,
        child: &'a ClassDecl,
        methods: &Methods<'a>,
    ) -> Result<bool, Box<Linked>> {
        if child.traits.is_empty() || child.kind == ClassKind::Trait {
            return Ok(false);
        }
        let Some(&(None, own)) = methods.find(b"__toString", member_name) else {
            return Ok(false);
        };
        let Some(stringable) = self.class_named(b"Stringable") else {
            return Ok(false);
        };
        let mut all = Vec::new();
        self.interfaces_of(&child.interfaces, &mut all, 0);
        let through_parent = self
            .class_parent(child)
            .is_some_and(|parent| self.instance_of(parent, stringable));
        if through_parent || all.iter().any(|interface| same(interface, stringable)) {
            return Ok(false);
        }
        if let Some(proto) = stringable.methods.iter().find(|m| same_method(m, own)) {
            let (fe_side, proto_side) = Self::own_over(child, stringable);
            if let Some(linked) =
                self.method_refused(child, own, fe_side, proto, proto_side, Check::Interface)
            {
                return Err(Box::new(linked));
            }
        }
        Ok(true)
    }

    /// `zend_do_inheritance_ex`: the parent itself, then its properties,
    /// its constants and its methods against the child's, the child's
    /// table taking what it does not redeclare.
    fn inherit(
        &mut self,
        child: &'a ClassDecl,
        parent: &'a ClassDecl,
        constants: &mut Constants<'a>,
        methods: &mut Methods<'a>,
    ) -> Option<Linked> {
        if let Some(refusal) = Self::parent_refused(child, parent) {
            return Some(Linked::Refused(at_class(child, refusal)));
        }
        let inherited = self.methods_above(parent);
        for &(declaring, property) in self.members_above::<PropertyDecl>(parent).rows() {
            let declaring = declaring.unwrap_or(parent);
            if let Some(own) = child.properties.iter().find(|p| p.name == property.name)
                && let Some(linked) =
                    self.inherited_property_refused(child, own, property, declaring, Check::Parent)
            {
                return Some(linked);
            }
        }
        if let Some(linked) = self.hooked_properties_refused(child) {
            return Some(linked);
        }
        let above = self.members_above::<ClassConstDecl>(parent);
        for &(declaring, constant) in above.rows() {
            let declaring = declaring.unwrap_or(parent);
            match constants.find(&constant.name.0, member_name).copied() {
                Some((held_by, held)) => {
                    if let Some(linked) =
                        self.constant_refused(child, held, held_by, constant, declaring)
                    {
                        return Some(linked);
                    }
                }
                None if constant.visibility != Visibility::Private => {
                    put(constants, Some(declaring), constant);
                }
                None => {}
            }
        }
        for (declaring, method) in inherited {
            match methods.find(&method.function.name.0, member_name).copied() {
                Some((None, own)) => {
                    let (declaring, method) = self.prototype(declaring, method);
                    let (fe_side, proto_side) = Self::own_over(child, declaring);
                    if let Some(linked) =
                        self.method_refused(child, own, fe_side, method, proto_side, Check::Parent)
                    {
                        return Some(linked);
                    }
                }
                Some(_) => {}
                None => put(methods, Some(declaring), method),
            }
        }
        None
    }

    /// `parent->common.prototype` where the parent's method is a
    /// constructor that is not abstract: the abstract constructor it
    /// was held to, an interface's or one up the chain, which is what a
    /// child's constructor is held to in its place
    /// (`do_inheritance_check_on_method`); the method itself otherwise.
    ///
    /// ```php
    /// abstract class A { abstract function __construct(int $x); }
    /// class B extends A { function __construct(int|string $x) {} }
    /// class C extends B { function __construct(string $x) {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Declaration of C::__construct(string $x) must be compatible with A::__construct(int $x) in %s on line 4
    /// ```
    fn prototype(
        &self,
        declaring: &'a ClassDecl,
        method: &'a MethodDecl,
    ) -> (&'a ClassDecl, &'a MethodDecl) {
        let ctor = |class: &'a ClassDecl| {
            class
                .methods
                .iter()
                .find(|m| m.function.name.0.eq_ignore_ascii_case(b"__construct"))
        };
        if method.is_abstract || !method.function.name.0.eq_ignore_ascii_case(b"__construct") {
            return (declaring, method);
        }
        let mut at = declaring;
        for _ in 0..64 {
            let mut interfaces = Vec::new();
            self.interfaces_of(&at.interfaces, &mut interfaces, 0);
            if let Some(found) = interfaces
                .into_iter()
                .rev()
                .find_map(|i| ctor(i).map(|m| (i, m)))
            {
                return found;
            }
            let Some(parent) = self.class_parent(at) else {
                break;
            };
            match ctor(parent) {
                Some(m) if m.is_abstract => return (parent, m),
                _ => at = parent,
            }
        }
        (declaring, method)
    }

    /// `zend_do_implement_interfaces`: each interface the class names,
    /// refused where it is not one or is named twice, then each of them
    /// and each one's parents, `do_interface_implementation` over its
    /// constants and methods and then `interface_gets_implemented`.
    ///
    /// A method another interface or a parent put in the table is
    /// held to the interface's as the class's own is:
    ///
    /// ```php
    /// interface d { static function B(); }
    /// interface c { function b(); }
    /// interface a extends d, c {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot make non static method c::B() static in class d in %s on line 2
    /// ```
    fn implement(
        &mut self,
        child: &'a ClassDecl,
        constants: &mut Constants<'a>,
        methods: &mut Methods<'a>,
    ) -> Option<Linked> {
        let mut named: Vec<&ClassDecl> = Vec::new();
        for name in &child.interfaces {
            let Some(interface) = self.class_named(&name.0) else {
                continue;
            };
            self.depend(name);
            if interface.kind != ClassKind::Interface {
                return Some(Linked::Refused(at_class(
                    child,
                    Refusal::NotAnInterface {
                        class: text(&child.name.0),
                        interface: text(&interface.name.0),
                    },
                )));
            }
            if named.iter().any(|seen| same(seen, interface)) {
                return Some(Linked::Refused(at_class(
                    child,
                    Refusal::PreviouslyImplemented {
                        kind: child.kind,
                        class: text(&child.name.0),
                        interface: text(&interface.name.0),
                    },
                )));
            }
            named.push(interface);
        }
        let all = self.all_interfaces(child);
        if let Some(refusal) = self.inherited_interfaces_refused(child, &all) {
            return Some(Linked::Refused(at_class(child, refusal)));
        }
        let inherited = self
            .class_parent(child)
            .map(|parent| self.all_interfaces(parent))
            .unwrap_or_default();
        let mut interfaces = Vec::new();
        self.interfaces_of(&child.interfaces, &mut interfaces, 0);
        for interface in interfaces {
            if named.iter().any(|own| same(own, interface)) {
                let again = inherited.iter().any(|seen| same(seen, interface));
                if let Some(linked) =
                    self.implementation_refused(child, interface, again, constants, methods)
                {
                    return Some(linked);
                }
            }
            if child.kind != ClassKind::Interface
                && let Some(refusal) = self.interface_refused(child, interface, &all)
            {
                return Some(Linked::Refused(at_class(child, refusal)));
            }
        }
        None
    }

    /// `do_interface_implementation` of one interface the class names,
    /// over the interface's linked tables, in which a constant or a
    /// method it redeclares hides its parent's; the parents it extends
    /// add nothing (`zend_do_inherit_interfaces`). One its parent
    /// implements `again` is only `do_inherit_constant_check`ed.
    ///
    /// ```php
    /// interface I { const X = "i"; function m(int $a); }
    /// interface I2 extends I { const X = "i2"; function m(int|string $a); }
    /// class D implements I2 { function m(int|string $a) {} }
    /// echo D::X, "\n";
    /// interface K { const Y = "k"; }
    /// class P implements K {}
    /// class Q extends P implements K { const Y = "q"; }
    /// echo Q::Y, "\n";
    /// abstract class E implements I2, I {}
    /// ```
    /// ```expect exit=255
    /// i2
    /// q
    ///
    /// Fatal error: Class E inherits both I2::X and I::X, which is ambiguous in %s on line 10
    /// ```
    ///
    /// ```php
    /// interface I { const X = "i"; }
    /// interface J { const X = "j"; }
    /// class D implements I, J {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class D inherits both I::X and J::X, which is ambiguous in %s on line 4
    /// ```
    fn implementation_refused(
        &mut self,
        child: &'a ClassDecl,
        interface: &'a ClassDecl,
        again: bool,
        constants: &mut Constants<'a>,
        methods: &mut Methods<'a>,
    ) -> Option<Linked> {
        for &(declaring, constant) in self.members_above::<ClassConstDecl>(interface).rows() {
            let declaring = declaring.unwrap_or(interface);
            match constants.find(&constant.name.0, member_name).copied() {
                Some((held_by, _)) if held_by.is_some_and(|by| same(by, declaring)) => {}
                Some((held_by, held)) => {
                    let refused = self.constant_refused(child, held, held_by, constant, declaring);
                    if refused.is_some() {
                        return refused;
                    }
                }
                None if !again => put(constants, Some(declaring), constant),
                None => {}
            }
        }
        if again {
            return None;
        }
        for &(declaring, method) in self.members_above::<MethodDecl>(interface).rows() {
            let declaring = declaring.unwrap_or(interface);
            match methods.find(&method.function.name.0, member_name).copied() {
                Some((None, own)) => {
                    let (fe_side, proto_side) = Self::own_over(child, declaring);
                    let check = Check::Interface;
                    let refused =
                        self.method_refused(child, own, fe_side, method, proto_side, check);
                    if refused.is_some() {
                        return refused;
                    }
                }
                // `do_inherit_method` over a method another class of the
                // chain or another interface put there: held to the
                // interface's as php holds it, unless it is that very
                // method.
                Some((Some(held_by), held)) if !same(held_by, declaring) => {
                    let fe_side = Side {
                        scope: Self::scope_of(child, Cls::Linked(held_by)),
                        shown: &held_by.name.0,
                    };
                    let (_, proto_side) = Self::own_over(child, declaring);
                    let check = Check::Interface;
                    let refused =
                        self.method_refused(child, held, fe_side, method, proto_side, check);
                    if refused.is_some() {
                        return refused;
                    }
                }
                Some(_) => {}
                None => put(methods, Some(declaring), method),
            }
        }
        for &(declaring, property) in self.members_above::<PropertyDecl>(interface).rows() {
            let declaring = declaring.unwrap_or(interface);
            if let Some(own) = child.properties.iter().find(|p| p.name == property.name)
                && let Some(linked) = self.inherited_property_refused(
                    child,
                    own,
                    property,
                    declaring,
                    Check::Interface,
                )
            {
                return Some(linked);
            }
        }
        None
    }

    /// `zend_do_inheritance`'s `do_implement_interface` over what the
    /// parent implements, whose guards hold the child too, before its
    /// own interfaces'.
    fn inherited_interfaces_refused(
        &self,
        child: &'a ClassDecl,
        all: &[&'a ClassDecl],
    ) -> Option<Refusal> {
        if child.kind == ClassKind::Interface {
            return None;
        }
        let mut own = Vec::new();
        self.interfaces_of(&child.interfaces, &mut own, 0);
        all.iter()
            .filter(|interface| !own.iter().any(|o| same(o, interface)))
            .find_map(|interface| self.interface_refused(child, interface, all))
    }

    /// `ce->interfaces` once `zend_do_link_class` has resolved them: the
    /// interfaces each parent up the chain holds, then the class's own
    /// and those each one extends, each once.
    fn all_interfaces(&self, child: &'a ClassDecl) -> Vec<&'a ClassDecl> {
        let mut chain: Vec<&'a ClassDecl> = Vec::new();
        let mut at = self.class_parent(child);
        while let Some(parent) = at {
            if chain.len() > 64 || chain.iter().any(|seen| same(seen, parent)) {
                break;
            }
            chain.push(parent);
            at = self.class_parent(parent);
        }
        let mut all = Vec::new();
        for class in chain.into_iter().rev() {
            self.interfaces_of(&class.interfaces, &mut all, 0);
        }
        self.interfaces_of(&child.interfaces, &mut all, 0);
        all
    }

    /// `interface_gets_implemented` of the interfaces php's engine
    /// guards: `Throwable` only through `Exception` or `Error`,
    /// `Traversable` only through `Iterator` or `IteratorAggregate`, the
    /// enum interfaces only on an enum and `BackedEnum` only on a backed
    /// one.
    ///
    /// Each guard runs over what the class holds once linked, its
    /// parents' interfaces included, as `ce->interfaces` is by then:
    ///
    /// ```php
    /// abstract class A implements IteratorAggregate, Iterator {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class A cannot implement both Iterator and IteratorAggregate at the same time in %s on line 2
    /// ```
    ///
    /// ```php
    /// abstract class A implements Traversable {}
    /// class B extends A {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class B must implement interface Traversable as part of either Iterator or IteratorAggregate in Unknown on line 0
    /// ```
    ///
    /// ```php
    /// enum Foo implements Traversable {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Enum Foo must implement interface Traversable as part of either Iterator or IteratorAggregate in Unknown on line 0
    /// ```
    ///
    /// An extension's interface that names the classes an implementor
    /// must descend from holds any other to it:
    ///
    /// ```php
    /// class A extends DateTime implements DateTimeInterface {}
    /// class B extends DateTimeImmutable {}
    /// echo "linked\n";
    /// class C implements DateTimeInterface {}
    /// ```
    /// ```expect exit=255
    /// linked
    ///
    /// Fatal error: DateTimeInterface can't be implemented by user classes in %s on line 5
    /// ```
    fn interface_refused(
        &self,
        child: &'a ClassDecl,
        interface: &ClassDecl,
        all: &[&'a ClassDecl],
    ) -> Option<Refusal> {
        let name = &interface.name.0;
        let class = text(&child.name.0);
        if name.eq_ignore_ascii_case(b"Throwable") {
            let through_throwable = child
                .parent
                .as_ref()
                .and_then(|p| self.class_named(&p.0))
                .is_some_and(|parent| self.instance_of(parent, interface));
            if !through_throwable {
                return Some(Refusal::Throwable {
                    kind: child.kind,
                    class,
                });
            }
        }
        let holds = |wanted: &[u8]| all.iter().any(|i| i.name.0.eq_ignore_ascii_case(wanted));
        if name.eq_ignore_ascii_case(b"Traversable")
            && !child.flags.is_abstract
            && !holds(b"Iterator")
            && !holds(b"IteratorAggregate")
        {
            return Some(Refusal::Traversable {
                kind: child.kind,
                class,
            });
        }
        let other: &[u8] = if name.eq_ignore_ascii_case(b"Iterator") {
            b"IteratorAggregate"
        } else {
            b"Iterator"
        };
        if (name.eq_ignore_ascii_case(b"Iterator")
            || name.eq_ignore_ascii_case(b"IteratorAggregate"))
            && holds(other)
        {
            return Some(Refusal::IteratorAndAggregate { class });
        }
        let descends = |implementor: &Name| {
            self.class_named(&implementor.0)
                .is_some_and(|implementor| self.instance_of(child, implementor))
        };
        if !interface.implementors.is_empty() && !interface.implementors.iter().any(descends) {
            return Some(Refusal::UserImplementor {
                interface: text(name),
            });
        }
        let backed = name.eq_ignore_ascii_case(b"BackedEnum");
        if !backed && !name.eq_ignore_ascii_case(b"UnitEnum") {
            return None;
        }
        if child.kind != ClassKind::Enum {
            return Some(Refusal::EnumInterface {
                class,
                interface: text(name),
            });
        }
        (backed && child.backing == EnumBacking::Pure).then(|| Refusal::NonBackedEnumInterface {
            class,
            interface: text(name),
        })
    }
}

impl<'a, H: Copy> Linker<'a, H> {
    /// `zend_implement_serializable`'s deprecation: a class that is
    /// `Serializable`, through its own interfaces or its parent's, and is
    /// not declared abstract, left without `__serialize()` or
    /// `__unserialize()` once it holds what it inherits. php raises it
    /// for every class of the line, the subclass of one too.
    ///
    /// ```php
    /// class A implements Serializable { function serialize() {} function unserialize($s) {} }
    /// class B extends A {}
    /// class C extends A { function __serialize(): array { return []; } function __unserialize(array $a): void {} }
    /// abstract class D implements Serializable {}
    /// echo "linked\n";
    /// ```
    /// ```expect
    /// Deprecated: A implements the Serializable interface, which is deprecated. Implement __serialize() and __unserialize() instead (or in addition, if support for old PHP versions is necessary) in %s on line 2
    ///
    /// Deprecated: B implements the Serializable interface, which is deprecated. Implement __serialize() and __unserialize() instead (or in addition, if support for old PHP versions is necessary) in %s on line 3
    /// linked
    /// ```
    fn serializable_deprecated(&mut self, child: &'a ClassDecl, methods: &Methods<'a>) {
        if matches!(child.kind, ClassKind::Interface | ClassKind::Trait) || child.flags.is_abstract
        {
            return;
        }
        let serializable = self
            .class_named(b"serializable")
            .is_some_and(|interface| self.instance_of(child, interface));
        let has = |name: &[u8]| methods.find(name, member_name).is_some();
        if serializable && !(has(b"__serialize") && has(b"__unserialize")) {
            self.raised.push(Raised::Serializable {
                class: text(&child.name.0),
                site: Site {
                    file: child.file.clone(),
                    line: child.line,
                },
            });
        }
    }
}

impl<'a, H: Copy> Linker<'a, H> {
    /// `zend_verify_enum_magic_methods` and `zend_verify_enum_interfaces`,
    /// after `zend_verify_enum_properties`: an enum holding a magic
    /// method php keeps off one, its own or a trait's, and an enum that
    /// is `Serializable`, whose deprecation
    /// `zend_implement_serializable` raised first.
    ///
    /// ```php
    /// trait T { public function __construct() {} }
    /// enum E { use T; case A; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Enum E cannot include magic method __construct in %s on line 3
    /// ```
    ///
    /// ```php
    /// enum E { case A; public function __toString(): string { return ''; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Enum E cannot include magic method __toString in %s on line 2
    /// ```
    ///
    /// ```php
    /// enum E implements Serializable { case A;
    ///     public function serialize() {} public function unserialize($s) {} }
    /// ```
    /// ```expect exit=255
    /// Deprecated: E implements the Serializable interface, which is deprecated. Implement __serialize() and __unserialize() instead (or in addition, if support for old PHP versions is necessary) in %s on line 2
    ///
    /// Fatal error: Enum E cannot implement the Serializable interface in %s on line 2
    /// ```
    fn enum_refused(&self, child: &'a ClassDecl) -> Option<Refusal> {
        const FORBIDDEN: [&str; 14] = [
            "__construct",
            "__destruct",
            "__clone",
            "__get",
            "__set",
            "__unset",
            "__isset",
            "__toString",
            "__debugInfo",
            "__serialize",
            "__unserialize",
            "__sleep",
            "__wakeup",
            "__set_state",
        ];
        if child.kind != ClassKind::Enum {
            return None;
        }
        let class = text(&child.name.0);
        let has = |wanted: &str| {
            child
                .methods
                .iter()
                .any(|m| m.function.name.0.eq_ignore_ascii_case(wanted.as_bytes()))
        };
        if let Some(method) = FORBIDDEN.into_iter().find(|magic| has(magic)) {
            return Some(Refusal::EnumMagic { class, method });
        }
        self.class_named(b"serializable")
            .is_some_and(|interface| self.instance_of(child, interface))
            .then_some(Refusal::EnumSerializable { class })
    }
}

/// Each of `keys` the list does not hold, at its end.
fn extend_new<'k, H: Copy + PartialEq + 'k>(list: &mut Vec<H>, keys: impl Iterator<Item = &'k H>) {
    for key in keys {
        if !list.contains(key) {
            list.push(*key);
        }
    }
}

/// A refusal at the class's own line, where php raises what is not
/// about one method.
fn at_class(child: &ClassDecl, refusal: Refusal) -> Refused {
    Refused {
        refusal,
        site: Site {
            file: child.file.clone(),
            line: child.line,
        },
    }
}

/// Two methods of one name, whatever the case.
fn same_method(a: &MethodDecl, b: &MethodDecl) -> bool {
    a.function.name.0.eq_ignore_ascii_case(&b.function.name.0)
}

/// `zend_verify_abstract_class` over the methods a class holds once
/// linked: a
/// class not declared abstract left with an abstract method, one it
/// inherited or one an interface declares, `E_ERROR` in php and a fatal
/// all the same. A class declared abstract is held to its private
/// abstract methods alone, which only it could implement, as a trait's
/// `abstract private` method leaves one.
///
/// ```php
/// trait T { abstract private function m(int $a); }
/// abstract class C { use T; }
/// ```
/// ```expect exit=255
/// Fatal error: Class C must implement 1 abstract method (C::m) in %s on line 3
/// ```
///
/// ```php
/// abstract class A { abstract function f(); abstract function g(); }
/// class B extends A { function f() {} }
/// ```
/// ```expect exit=255
/// Fatal error: Class B contains 1 abstract method and must therefore be declared abstract or implement the remaining method (A::g) in %s on line 3
/// ```
fn abstract_left(
    child: &ClassDecl,
    methods: &[(Option<&ClassDecl>, &MethodDecl)],
    hooks: Vec<String>,
) -> Option<Refusal> {
    if !matches!(child.kind, ClassKind::Class | ClassKind::Enum) {
        return None;
    }
    let explicit = child.flags.is_abstract;
    let mut left: Vec<String> = methods
        .iter()
        .filter(|(by, method)| {
            (method.is_abstract || by.is_some_and(|by| by.kind == ClassKind::Interface))
                && (!explicit || method.visibility == Visibility::Private)
        })
        .map(|(by, method)| {
            let scope = by.map_or(&child.name, |by| &by.name);
            format!("{}::{}", text(&scope.0), text(&method.function.name.0))
        })
        .collect();
    // A class declared abstract is not held to its properties' hooks.
    if !explicit {
        left.extend(hooks);
    }
    (!left.is_empty()).then(|| Refusal::AbstractLeft {
        kind: child.kind,
        class: text(&child.name.0),
        methods: left,
        // `can_be_abstract`: an enum and an anonymous class cannot be
        // declared abstract, so neither is told it could.
        must_implement: explicit || child.kind == ClassKind::Enum || child.flags.is_anonymous,
    })
}

/// `zend_verify_enum_properties`: an enum holds no property but its
/// `name` and a backed one's `value`, which is what a trait's property
/// is refused by once the trait put it there.
///
/// ```php
/// trait A { public $x; }
/// enum E { use A; case X; }
/// ```
/// ```expect exit=255
/// Fatal error: Enum E cannot include properties in %s on line 3
/// ```
fn enum_properties(child: &ClassDecl) -> Option<Refusal> {
    if child.kind != ClassKind::Enum {
        return None;
    }
    child
        .properties
        .iter()
        .any(|p| {
            !(&*p.name.0 == b"name"
                || (child.backing != EnumBacking::Pure && &*p.name.0 == b"value"))
        })
        .then(|| Refusal::EnumProperties {
            class: text(&child.name.0),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_is_written_as_php_writes_it_in_a_declaration() {
        assert_eq!(
            default_text(&ConstExpr::Str(b"llllllllllll"[..].into())),
            "'llllllllll...'"
        );
        assert_eq!(
            default_text(&ConstExpr::Array {
                elements: Vec::new(),
                line: 0
            }),
            "[]"
        );
        assert_eq!(default_text(&ConstExpr::Null), "null");
    }
}
