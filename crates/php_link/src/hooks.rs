//! Property hooks at the link (#427): `zend_inheritance.c`'s arms of
//! `do_inherit_property` over a hooked property (`inherit_property_hook`,
//! `prop_get_variance`, the `set` hook's type against an abstract
//! parent's), `zend_verify_hooked_property` once the class's own
//! properties stand, `zend_verify_property_hook_variance` over a `set`
//! hook's parameter, and the abstract hooks `zend_verify_abstract_class`
//! counts. What a linked class holds for a property is read by walking
//! the chain as every inherited member is ([`resolve_property`]), so the
//! engine and Reflection answer from the one resolution the checks
//! here are made over.

use super::{
    Check, Cls, Held, Linked, Linker, Refusal, Refused, Scope, Site, Status, at_class, resolved,
    text,
};
use php_unit::decl::{ClassDecl, HookKind, MethodDecl, PropertyDecl, Type, Visibility};
use php_unit::type_to_string;

/// One hook a linked class holds for a property: the class whose
/// declaration wrote it, which is its scope (`zend_duplicate_function`
/// keeps a user hook's), the property's place among that class's own,
/// and the hook itself.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedHook<'a> {
    pub class: &'a ClassDecl,
    pub property: u32,
    pub method: &'a MethodDecl,
}

/// A property as `ce->properties_info` holds it once linked: the class
/// whose declaration stands in the table (`property_info->ce`) and that
/// declaration, the hook of each kind the table's entry carries, its
/// own or one inherited, and whether it is still `ZEND_ACC_VIRTUAL`,
/// which it is only where every declaration of it up the chain is.
/// `root` is the class of the topmost declaration the chain overrides,
/// `prop_info->prototype`'s, which two hooked properties share when
/// one is the other's inherited line (`zend_is_in_hook`).
#[derive(Debug, Clone, Copy)]
pub struct Resolved<'a> {
    pub class: &'a ClassDecl,
    pub property: &'a PropertyDecl,
    pub get: Option<ResolvedHook<'a>>,
    pub set: Option<ResolvedHook<'a>>,
    pub is_virtual: bool,
    pub root: &'a ClassDecl,
}

impl<'a> Resolved<'a> {
    /// The hook of `kind`, `prop_info->hooks[kind]`.
    #[must_use]
    pub fn hook(&self, kind: HookKind) -> Option<ResolvedHook<'a>> {
        match kind {
            HookKind::Get => self.get,
            HookKind::Set => self.set,
        }
    }

    /// Whether the table's entry has hooks at all, `prop_info->hooks`.
    #[must_use]
    pub fn is_hooked(&self) -> bool {
        self.get.is_some() || self.set.is_some()
    }
}

/// The declarations of one property up a class's parents, the most
/// derived first: the one the class finds by name, then each parent's
/// that it overrides, which a parent's private one never is.
fn declarations<'a>(
    class: &'a ClassDecl,
    name: &[u8],
    lookup: &dyn Fn(&[u8]) -> Option<Held<'a>>,
) -> Vec<(&'a ClassDecl, u32, &'a PropertyDecl)> {
    let mut out: Vec<(&'a ClassDecl, u32, &'a PropertyDecl)> = Vec::new();
    let mut at = Some(class);
    let mut depth = 0;
    while let Some(here) = at
        && depth < 64
    {
        if let Some(index) = here.properties.iter().position(|p| &*p.name.0 == name) {
            let property = &here.properties[index];
            if !out.is_empty() && property.visibility == Visibility::Private {
                break;
            }
            out.push((here, u32::try_from(index).unwrap_or(u32::MAX), property));
        }
        depth += 1;
        at = here
            .parent
            .as_ref()
            .and_then(|parent| lookup(&parent.0))
            .map(|held| held.decl);
    }
    out
}

/// `ce->properties_info[name]` of a linked class: the declaration the
/// class finds, with each hook `inherit_property_hook` leaves on it,
/// its own or the nearest parent's, and the virtual bit
/// `do_inherit_property` clears where any declaration above is backed.
/// An abstract hook a backed declaration below it implements is none,
/// since the backing value serves what it declares. None for a name no
/// class of the chain declares.
///
/// ```php
/// class A { public $x = 1 { get => $this->x + 1; } }
/// class B extends A { public $x = 5; }
/// var_dump((new B)->x);
/// ```
/// ```expect
/// int(6)
/// ```
#[must_use]
pub fn resolve_property<'a>(
    class: &'a ClassDecl,
    name: &[u8],
    lookup: &dyn Fn(&[u8]) -> Option<Held<'a>>,
) -> Option<Resolved<'a>> {
    let chain = declarations(class, name, lookup);
    let &(declaring, _, property) = chain.first()?;
    let &(root, _, _) = chain.last()?;
    let hook = |kind: HookKind| {
        for (at, &(class, index, property)) in chain.iter().enumerate() {
            let Some(method) = property.hooks.as_ref().and_then(|hooks| hooks.of(kind)) else {
                continue;
            };
            // `inherit_property_hook`: a backed declaration below an
            // abstract hook has the operation it declares, and takes no
            // copy of it.
            if method.is_abstract
                && chain[..at].iter().any(|(_, _, below)| {
                    !below.is_virtual && (kind == HookKind::Get || !below.is_readonly)
                })
            {
                return None;
            }
            return Some(ResolvedHook {
                class,
                property: index,
                method,
            });
        }
        None
    };
    Some(Resolved {
        class: declaring,
        property,
        get: hook(HookKind::Get),
        set: hook(HookKind::Set),
        is_virtual: chain.iter().all(|(_, _, p)| p.is_virtual),
        root,
    })
}

/// What `zend_verify_property_hook_variance` comes to over a `set`
/// hook while the file compiles: the property's type a subtype of the
/// parameter's, or a class it names not held yet, which
/// `zend_compile_property_hooks` counts in
/// `num_hooked_prop_variance_checks` for the link to check again, or
/// the refusal php raises before the class is declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookVariance {
    Holds,
    Unresolved,
    Refused(Refusal),
}

/// `zend_verify_property_hook_variance` as `zend_compile_property_hooks`
/// runs it once a `set` hook has compiled, against the classes `lookup`
/// holds while the file compiles and the class itself, in no table yet,
/// from its own scope ([`Standing::Compiling`](super::Standing)).
///
/// ```php
/// echo "not reached\n";
/// class P {}
/// class C extends P { public int $x { set(string $v) {} } }
/// ```
/// ```expect exit=255
/// Fatal error: Type of parameter $v of hook C::$x::set must be compatible with property type in %s on line 4
/// ```
///
/// ```php
/// class A {} class B {}
/// class C extends P { public A $x { set(B $v) {} } }
/// ```
/// ```expect exit=255
/// Fatal error: Type of parameter $v of hook C::$x::set must be compatible with property type in %s on line 3
/// ```
///
/// A class the check names that is declared later leaves it to the
/// link, where the class is declared:
///
/// ```php
/// echo "reached\n";
/// class Test { public X $prop { set(Y $prop) {} } }
/// interface X {}
/// interface Y {}
/// ```
/// ```expect exit=255
/// reached
///
/// Fatal error: Type of parameter $prop of hook Test::$prop::set must be compatible with property type in %s on line 3
/// ```
#[must_use]
pub fn set_hook_variance<'a>(
    class: &'a ClassDecl,
    property: &[u8],
    ty: Option<&Type>,
    set: &MethodDecl,
    lookup: &'a dyn Fn(&[u8]) -> Option<Held<'a>>,
) -> HookVariance {
    let mut linker = Linker::over(lookup, &|()| None, Vec::new());
    linker.standing = super::Standing::Compiling;
    match linker.set_hook_variance(class, property, ty, set) {
        Some((Status::Error, refused)) => HookVariance::Refused(refused.refusal),
        Some((Status::Unresolved, _)) => HookVariance::Unresolved,
        Some((Status::Success | Status::Warning, _)) | None => HookVariance::Holds,
    }
}

/// `zend_verify_hooked_property`: what a hooked property of `class` is
/// refused for once its class's link has settled whether it is virtual,
/// in php's order. None for one with no hooks.
///
/// ```php
/// abstract class A { abstract public $x { get => 1; } }
/// ```
/// ```expect exit=255
/// Fatal error: Abstract property A::$x must specify at least one abstract hook in %s on line 2
/// ```
///
/// ```php
/// class A { public private(set) int $x { get => 1; } }
/// ```
/// ```expect exit=255
/// Fatal error: Read-only virtual property A::$x must not specify asymmetric visibility in %s on line 2
/// ```
#[must_use]
pub fn verify_hooked_property(class: &[u8], property: &PropertyDecl) -> Option<Refusal> {
    let hooks = property.hooks.as_ref()?;
    let (class, name) = (text(class), text(&property.name.0));
    if property.is_virtual && property.default.is_some() {
        return Some(Refusal::VirtualDefault {
            class,
            property: name,
        });
    }
    if hooks
        .get
        .as_ref()
        .is_some_and(|get| get.function.returns_by_ref)
        && !property.is_virtual
        && hooks.set.is_some()
    {
        return Some(Refusal::BackedGetByRef {
            class,
            property: name,
        });
    }
    let abstract_hook = [&hooks.get, &hooks.set]
        .into_iter()
        .flatten()
        .any(|hook| hook.is_abstract);
    if hooks.is_abstract && !abstract_hook {
        return Some(Refusal::AbstractWithoutAbstractHook {
            class,
            property: name,
        });
    }
    if property.is_virtual
        && property.set_visibility.is_some()
        && (hooks.get.is_none() || hooks.set.is_none())
    {
        return Some(Refusal::VirtualAsymmetric {
            write_only: hooks.get.is_none(),
            class,
            property: name,
        });
    }
    None
}

/// `prop_get_variance`: how a child's type is held to a parent's, a
/// virtual property with only a `get` hook read covariantly and one with
/// only a `set` hook contravariantly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variance {
    Invariant,
    Covariant,
    Contravariant,
}

impl<'a, H: Copy> Linker<'a, H> {
    /// The two hooks of a parent's hooked property that the child's
    /// declaration of it is held to, in php's order: `inherit_property_hook`
    /// for each kind, a parent's `final` hook refused and a hook the
    /// child writes checked against the parent's as a method is
    /// (`do_inheritance_check_on_method`), whose sentence names both as
    /// `$name::get`.
    ///
    /// ```php
    /// class A { public $prop { final get { return 42; } } }
    /// class B extends A { public $prop { get { return 24; } } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot override final property hook A::$prop::get() in %s on line 3
    /// ```
    ///
    /// ```php
    /// class A { public int $a { get {} } }
    /// class B extends A { public int|float $a { get {} } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Declaration of B::$a::get(): int|float must be compatible with A::$a::get(): int in %s on line 3
    /// ```
    pub(super) fn hooks_refused(
        &mut self,
        child: &'a ClassDecl,
        own: &'a PropertyDecl,
        parent: &'a PropertyDecl,
        parent_class: &'a ClassDecl,
        check: Check,
    ) -> Option<Linked> {
        for kind in HookKind::ALL {
            let Some(proto) = parent.hooks.as_ref().and_then(|hooks| hooks.of(kind)) else {
                continue;
            };
            let Some(fe) = own.hooks.as_ref().and_then(|hooks| hooks.of(kind)) else {
                continue;
            };
            if proto.visibility == Visibility::Private {
                continue;
            }
            if proto.is_final {
                return Some(Linked::Refused(Refused {
                    refusal: Refusal::FinalHook {
                        parent: text(&parent_class.name.0),
                        hook: text(&proto.function.name.0),
                    },
                    site: Site {
                        file: fe.function.file.clone(),
                        line: fe.function.line,
                    },
                }));
            }
            let (fe_side, proto_side) = Self::own_over(child, parent_class);
            if let Some(linked) = self.method_refused(child, fe, fe_side, proto, proto_side, check)
            {
                return Some(linked);
            }
        }
        None
    }

    /// `verify_property_type_compatibility` under `prop_get_variance`
    /// of the parent, then, over an abstract parent with a `set` hook
    /// the child does not write, the child's type held to that hook's
    /// parameter.
    ///
    /// ```php
    /// class A { public int|string $prop { get => 1; } }
    /// class B extends A { public int $prop { get => 2; } }
    /// class C extends A { public int|string|array $prop; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type of C::$prop must be subtype of string|int (as in class A) in %s on line 4
    /// ```
    pub(super) fn hooked_type_refused(
        &self,
        child: &'a ClassDecl,
        own: &'a PropertyDecl,
        parent: &'a PropertyDecl,
        parent_class: &'a ClassDecl,
    ) -> Option<(Refusal, Status)> {
        let variance = match &parent.hooks {
            Some(hooks) if parent.is_virtual && hooks.set.is_none() => Variance::Covariant,
            Some(hooks) if parent.is_virtual && hooks.get.is_none() => Variance::Contravariant,
            _ => Variance::Invariant,
        };
        let fe_scope = Self::scope_of(child, Cls::Linking);
        let proto_scope = Self::scope_of(child, Cls::Linked(parent_class));
        let (class, property, parent_name) = (
            text(&child.name.0),
            text(&own.name.0),
            text(&parent_class.name.0),
        );
        if let Some(parent_ty) = &parent.ty {
            let status = match &own.ty {
                None => Status::Error,
                Some(child_ty) => self.variant(
                    child,
                    child_ty,
                    &fe_scope,
                    parent_ty,
                    &proto_scope,
                    variance,
                ),
            };
            if status != Status::Success {
                return Some((
                    Refusal::PropertyType {
                        class,
                        property,
                        ty: type_to_string(&resolved(parent_ty, &proto_scope)),
                        parent: parent_name,
                        variance,
                    },
                    status,
                ));
            }
        } else if own.ty.is_some() {
            return Some((
                Refusal::PropertyTypeOmitted {
                    class,
                    property,
                    parent: parent_name,
                },
                Status::Error,
            ));
        }
        let parent_hooks = parent.hooks.as_ref()?;
        if !parent_hooks.is_abstract {
            return None;
        }
        let set = parent_hooks.set.as_ref()?;
        if own.hooks.as_ref().is_some_and(|hooks| hooks.set.is_some()) {
            return None;
        }
        let set_ty = set.function.parameters.first()?.ty.as_ref()?;
        let status = match &own.ty {
            Some(child_ty) => self.covariant(child, &proto_scope, set_ty, &fe_scope, child_ty),
            None => Status::Error,
        };
        (status != Status::Success).then(|| {
            (
                Refusal::SetHookType {
                    class,
                    property,
                    ty: type_to_string(&resolved(set_ty, &proto_scope)),
                    parent_kind: parent_class.kind,
                    parent: parent_name,
                },
                status,
            )
        })
    }

    /// A child's type against a parent's under `variance`: equal or a
    /// subtype both ways where invariant, a subtype where covariant, a
    /// supertype where contravariant.
    fn variant(
        &self,
        child: &'a ClassDecl,
        child_ty: &Type,
        fe_scope: &Scope<'a>,
        parent_ty: &Type,
        proto_scope: &Scope<'a>,
        variance: Variance,
    ) -> Status {
        if variance == Variance::Invariant
            && resolved(child_ty, fe_scope) == resolved(parent_ty, proto_scope)
        {
            return Status::Success;
        }
        let down = || self.covariant(child, fe_scope, child_ty, proto_scope, parent_ty);
        let up = || self.covariant(child, proto_scope, parent_ty, fe_scope, child_ty);
        let (down, up) = match variance {
            Variance::Covariant => (down(), Status::Success),
            Variance::Contravariant => (Status::Success, up()),
            Variance::Invariant => (down(), up()),
        };
        if down == Status::Error || up == Status::Error {
            Status::Error
        } else if down == Status::Unresolved || up == Status::Unresolved {
            Status::Unresolved
        } else {
            Status::Success
        }
    }

    /// `zend_verify_property_hook_variance` of one `set` hook of the
    /// class's own: the property's type a subtype of the parameter's
    /// where the parameter is typed, and an untyped property's typed
    /// parameter refused. The status, with the refusal it raises.
    fn set_hook_variance(
        &self,
        child: &'a ClassDecl,
        property: &[u8],
        ty: Option<&Type>,
        set: &MethodDecl,
    ) -> Option<(Status, Refused)> {
        let parameter = set.function.parameters.first()?;
        let param_ty = parameter.ty.as_ref()?;
        let scope = Self::scope_of(child, Cls::Linking);
        let status = match ty {
            Some(ty) => self.covariant(child, &scope, ty, &scope, param_ty),
            None => Status::Error,
        };
        let refused = at_class(
            child,
            Refusal::SetHookParameter {
                parameter: text(&parameter.name.0),
                class: text(&child.name.0),
                property: text(property),
            },
        );
        Some((status, refused))
    }

    /// `zend_do_link_class`'s check of the class's own `set` hooks under
    /// `num_hooked_prop_variance_checks`, after the interfaces, the
    /// abstract methods and the enum, and before `Stringable` is added:
    /// what the compiler could not resolve ([`set_hook_refused`]),
    /// checked against the classes declared by now, and the obligation
    /// it leaves where one still is not. Checking every own hook again
    /// is checking those, since what held while the file compiled holds
    /// with more classes known and what was refused never reached here.
    ///
    /// ```php
    /// class Test { public string|array $prop { set(string $prop) {} } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Type of parameter $prop of hook Test::$prop::set must be compatible with property type in %s on line 2
    /// ```
    ///
    /// A class refused both here and by its interface is refused by the
    /// interface, where the hook's classes were declared after the class
    /// compiled:
    ///
    /// ```php
    /// interface I { function f(); }
    /// class C implements I { public A $x { set(B $v) {} } }
    /// class A {} class B {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C contains 1 abstract method and must therefore be declared abstract or implement the remaining method (I::f) in %s on line 3
    /// ```
    pub(super) fn set_hooks_refused(&self, child: &'a ClassDecl) -> Option<Linked> {
        for property in &child.properties {
            let Some(set) = property.hooks.as_ref().and_then(|hooks| hooks.set.as_ref()) else {
                continue;
            };
            let Some((status, refused)) =
                self.set_hook_variance(child, &property.name.0, property.ty.as_ref(), set)
            else {
                continue;
            };
            match status {
                Status::Success | Status::Warning => {}
                Status::Unresolved => {
                    self.defer(refused);
                }
                Status::Error => return Some(Linked::Refused(refused)),
            }
        }
        None
    }

    /// The abstract hooks a linked class is left with, each named as
    /// `zend_verify_abstract_class` names one, its scope and its name:
    /// for every property the class holds, in `properties_info`'s order
    /// ([`Linker::members_above`]), the hook of each kind the table's
    /// entry carries once `inherit_property_hook` has done its work,
    /// which an abstract one a backed declaration implements is not.
    ///
    /// ```php
    /// interface I { public $prop { get; set; } }
    /// class C implements I {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C contains 2 abstract methods and must therefore be declared abstract or implement the remaining methods (I::$prop::get, I::$prop::set) in %s on line 3
    /// ```
    ///
    /// The parent's table holds its interfaces' properties after its
    /// chain's, and the class's own interfaces' come after the whole of
    /// it:
    ///
    /// ```php
    /// interface I { public $a { get; } }
    /// abstract class P implements I { abstract public $p { get; } }
    /// interface J { public $b { get; } }
    /// abstract class Q extends P {}
    /// class C extends Q implements J {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C contains 3 abstract methods and must therefore be declared abstract or implement the remaining methods (P::$p::get, I::$a::get, J::$b::get) in %s on line 6
    /// ```
    pub(super) fn abstract_hooks(&self, child: &'a ClassDecl) -> Vec<String> {
        let mut sources: Vec<&'a ClassDecl> = vec![child];
        let mut above = self.class_parent(child);
        let mut depth = 0;
        while let Some(class) = above
            && depth < 64
        {
            sources.push(class);
            above = self.class_parent(class);
            depth += 1;
        }
        let mut interfaces = Vec::new();
        for class in &sources {
            self.interfaces_of(&class.interfaces, &mut interfaces, 0);
        }
        let mut out = Vec::new();
        for &(_, property) in self.members_above::<PropertyDecl>(child).rows() {
            let name = &property.name.0;
            for kind in HookKind::ALL {
                if let Some((class, hook)) = self.abstract_hook_of(child, name, kind, &interfaces) {
                    out.push(format!(
                        "{}::{}",
                        text(&class.name.0),
                        text(&hook.function.name.0)
                    ));
                }
            }
        }
        out
    }

    /// The hook of `kind` the table's entry for `name` carries once the
    /// parent and then each interface have been inherited, where it is
    /// abstract.
    fn abstract_hook_of(
        &self,
        child: &'a ClassDecl,
        name: &[u8],
        kind: HookKind,
        interfaces: &[&'a ClassDecl],
    ) -> Option<(&'a ClassDecl, &'a MethodDecl)> {
        let lookup = |n: &[u8]| (self.lookup)(n).map(Held::plain);
        if let Some(resolved) = resolve_property(child, name, &lookup) {
            if let Some(hook) = resolved.hook(kind) {
                return hook.method.is_abstract.then_some((hook.class, hook.method));
            }
            // `property_has_operation`: a backed property reads, and
            // writes unless it is readonly, with no hook.
            if !resolved.is_virtual && (kind == HookKind::Get || !resolved.property.is_readonly) {
                return None;
            }
        }
        interfaces.iter().find_map(|interface| {
            interface
                .properties
                .iter()
                .find(|p| &*p.name.0 == name)
                .and_then(|p| p.hooks.as_ref())
                .and_then(|hooks| hooks.of(kind))
                .map(|hook| (*interface, hook))
        })
    }

    /// The hooked properties of the class's own declarations held to
    /// `zend_verify_hooked_property` once the parent has been inherited,
    /// each over the virtual bit the chain leaves it.
    pub(super) fn hooked_properties_refused(&self, child: &'a ClassDecl) -> Option<Linked> {
        let lookup = |n: &[u8]| (self.lookup)(n).map(Held::plain);
        for property in child.properties.iter().filter(|p| p.hooks.is_some()) {
            let is_virtual = resolve_property(child, &property.name.0, &lookup)
                .is_some_and(|resolved| resolved.is_virtual);
            let linked = PropertyDecl {
                is_virtual: is_virtual && property.is_virtual,
                ..property.clone()
            };
            if let Some(refusal) = verify_hooked_property(&child.name.0, &linked) {
                return Some(Linked::Refused(at_class(child, refusal)));
            }
        }
        None
    }

    /// Whether a parent's set visibility binds the child: not over a
    /// virtual property with no `set` hook, which has none to hold.
    pub(super) fn set_visibility_binds(parent: &PropertyDecl) -> bool {
        !(parent.is_virtual
            && parent
                .hooks
                .as_ref()
                .is_some_and(|hooks| hooks.set.is_none()))
    }
}
