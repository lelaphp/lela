//! `zend_do_bind_traits`, the part of `zend_do_link_class` that runs
//! before the parent is inherited (`Z/Zend/zend_inheritance.c:3650`):
//! the adaptations resolved and held to php's refusals
//! (`zend_traits_init_trait_structures`), then each used trait's
//! methods (`zend_do_traits_method_binding`), constants
//! (`zend_do_traits_constant_binding`) and properties
//! (`zend_do_traits_property_binding`) put into the class's own tables,
//! in php's order and with php's conflict rules. What is put there is
//! the class's own from then on, since `zend_fixup_trait_method` gives
//! every copied method the class as its scope, so the checks against
//! the parent and the interfaces are made over the composed declaration
//! as over any other. A trait's abstract methods are bound after the
//! parent, by the linker, from the copies made here.

use super::{Body, Collision, Held, Raised, Refusal, Refused, Site, at_class, text};
use php_unit::decl::{
    AliasModifier, ClassConstDecl, ClassDecl, ClassKind, ConstExpr, MethodDecl, Name, PropertyDecl,
    TraitAdaptation, TraitMethodRef, Visibility,
};

/// One method copied from a trait as `zend_traits_copy_functions` copies
/// it: the lowered name it goes in under, the method renamed and with
/// the alias's modifier, the name it had in the trait, the body it
/// runs, and the trait it was copied from, which is its scope until it
/// is fixed up.
pub(super) struct Copy {
    pub(super) key: Vec<u8>,
    pub(super) method: MethodDecl,
    pub(super) original: Name,
    pub(super) body: Body,
    pub(super) from: Name,
}

/// The class composed with its traits' members, before its parent is
/// inherited: the declaration, the body of each of its methods, the
/// copies of the traits' abstract methods for the pass after the
/// parent, and what the composing raised.
pub(super) struct Composition {
    pub(super) decl: ClassDecl,
    pub(super) bodies: Vec<Body>,
    pub(super) abstracts: Vec<Copy>,
    pub(super) raised: Vec<Raised>,
}

/// Whose a method among the class's methods is while the traits are bound:
/// the class's own, or a trait's copy not yet fixed up, which is what
/// `zend_add_trait_method` tells a collision from an override by.
#[derive(Clone, PartialEq, Eq)]
enum Scope {
    Own,
    Trait(Name),
}

/// One row of the class's method table while the traits are bound.
struct Row {
    key: Vec<u8>,
    method: MethodDecl,
    body: Body,
    scope: Scope,
}

/// A trait the class uses, `traits_and_interfaces[i]`, none where the
/// class names one twice.
type Used<'a> = Option<Held<'a>>;

/// The binding in progress: the class, the table it binds against, the
/// traits it uses in order, what each `insteadof` excludes per trait,
/// the trait each `as` resolved to, and what was raised.
struct Composer<'a, 'l> {
    child: &'a ClassDecl,
    lookup: &'l dyn Fn(&[u8]) -> Option<Held<'a>>,
    /// `zval_update_constant_ex` at the link: the value an expression
    /// that is no plain value evaluates to, written as a plain value,
    /// where the linking caller can evaluate it; none at the compiler's early
    /// bind, which never composes a class that uses traits.
    values: &'l dyn Fn(&ConstExpr) -> Option<ConstExpr>,
    traits: Vec<Used<'a>>,
    excluded: Vec<Vec<Vec<u8>>>,
    aliases: Vec<Name>,
    raised: Vec<Raised>,
}

/// `zend_do_bind_traits` before the parent: `child` composed with the
/// traits `lookup` finds for it, or the first refusal in php's order
/// with what was raised before it. A trait named twice is used once,
/// and a trait that uses another composes it first, so what reaches a
/// class through two paths is one method with one body:
///
/// ```php
/// trait T {
///     public $p = 1;
///     const K = 'k';
///     function f() { return __CLASS__ . ' ' . self::class . ' ' . static::class; }
/// }
/// trait A { use T; }
/// trait B { use T; }
/// class C { use A, B, A; }
/// class D extends C {}
/// $d = new D;
/// var_dump($d->f(), $d->p, D::K, get_class_methods('C'));
/// ```
/// ```expect exit=0
/// string(5) "C C D"
/// int(1)
/// string(1) "k"
/// array(1) {
///   [0]=>
///   string(1) "f"
/// }
/// ```
pub(super) fn compose<'a>(
    child: &'a ClassDecl,
    lookup: &dyn Fn(&[u8]) -> Option<Held<'a>>,
    values: &dyn Fn(&ConstExpr) -> Option<ConstExpr>,
) -> Result<Composition, Box<(Refused, Vec<Raised>)>> {
    let mut traits: Vec<Used<'a>> = Vec::with_capacity(child.traits.len());
    for name in &child.traits {
        let found = lookup(&name.0);
        let repeated = found.is_some_and(|found| {
            traits
                .iter()
                .flatten()
                .any(|seen| same_class(seen.decl, found.decl))
        });
        traits.push(if repeated { None } else { found });
    }
    let mut composer = Composer {
        child,
        lookup,
        values,
        traits,
        excluded: vec![Vec::new(); child.traits.len()],
        aliases: Vec::new(),
        raised: Vec::new(),
    };
    match composer.compose() {
        Ok(composition) => Ok(composition),
        Err(refusal) => Err(Box::new((at_class(child, refusal), composer.raised))),
    }
}

fn same_class(a: &ClassDecl, b: &ClassDecl) -> bool {
    a.name.0.eq_ignore_ascii_case(&b.name.0)
}

fn lowered(name: &[u8]) -> Vec<u8> {
    name.to_ascii_lowercase()
}

/// Whether a trait's table has a method under the name.
fn has_method(held: &Held<'_>, name: &[u8]) -> bool {
    held.decl
        .methods
        .iter()
        .any(|m| m.function.name.0.eq_ignore_ascii_case(name))
}

impl<'a> Composer<'a, '_> {
    fn compose(&mut self) -> Result<Composition, Refusal> {
        self.init_trait_structures()?;
        let mut rows: Vec<Row> = self
            .child
            .methods
            .iter()
            .enumerate()
            .map(|(at, method)| Row {
                key: lowered(&method.function.name.0),
                method: method.clone(),
                body: Body {
                    class: self.child.name.clone(),
                    method: u32::try_from(at).unwrap_or(u32::MAX),
                },
                scope: Scope::Own,
            })
            .collect();
        let mut abstracts = Vec::new();
        for at in 0..self.traits.len() {
            let Some(used) = self.traits[at] else {
                continue;
            };
            for (index, method) in used.decl.methods.iter().enumerate() {
                let copies = self.copies(at, &used, index, method, !method.is_abstract);
                if method.is_abstract {
                    abstracts.extend(copies);
                    continue;
                }
                for copy in copies {
                    self.add_method(&mut rows, copy)?;
                }
            }
        }
        let mut decl = self.child.clone();
        decl.constants = self.bind_constants()?;
        decl.properties = self.bind_properties()?;
        let mut bodies = Vec::with_capacity(rows.len());
        decl.methods = rows
            .into_iter()
            .map(|row| {
                bodies.push(row.body);
                row.method
            })
            .collect();
        Ok(Composition {
            decl,
            bodies,
            abstracts,
            raised: std::mem::take(&mut self.raised),
        })
    }

    /// A trait an adaptation names, by the table: php's `Could not find
    /// trait` where it holds none.
    fn named_trait(&self, name: &Name) -> Result<Held<'a>, Refusal> {
        (self.lookup)(&name.0).ok_or_else(|| Refusal::TraitNotFound {
            name: text(&name.0),
        })
    }

    /// `zend_check_trait_usage`: the index of `held` among the traits
    /// the class uses, refused where it is no trait or not used.
    fn usage(&self, held: &Held<'a>) -> Result<usize, Refusal> {
        if held.decl.kind != ClassKind::Trait {
            return Err(Refusal::NotATrait {
                class: text(&held.decl.name.0),
            });
        }
        self.traits
            .iter()
            .position(|used| used.is_some_and(|used| same_class(used.decl, held.decl)))
            .ok_or_else(|| Refusal::TraitNotUsed {
                name: text(&held.decl.name.0),
                class: text(&self.child.name.0),
            })
    }

    /// `zend_traits_init_trait_structures`: every `insteadof` resolved
    /// into the exclude table of each trait it names, then every `as`
    /// resolved to the one trait whose method it adapts, each refusal
    /// php's compile error at the class's line.
    ///
    /// ```php
    /// trait A { function f() {} }
    /// class P {}
    /// class C { use A { P::f as g; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class P is not a trait, Only traits may be used in 'as' and 'insteadof' statements in %s on line 4
    /// ```
    ///
    /// ```php
    /// trait A { function f() {} }
    /// trait B { function f() {} }
    /// class C { use A { B::f as g; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Required Trait B wasn't added to C in %s on line 4
    /// ```
    ///
    /// ```php
    /// trait A { function f() {} }
    /// class C { use A { Nope::f as g; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Could not find trait Nope in %s on line 3
    /// ```
    ///
    /// ```php
    /// trait A { function f() {} }
    /// trait B { function f() {} }
    /// class C { use A, B { A::g insteadof B; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: A precedence rule was defined for A::g but this method does not exist in %s on line 4
    /// ```
    ///
    /// ```php
    /// trait A { function f() {} }
    /// trait B { function f() {} }
    /// class C { use A, B { A::f insteadof B, B; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Failed to evaluate a trait precedence (f). Method of trait B was defined to be excluded multiple times in %s on line 4
    /// ```
    ///
    /// ```php
    /// trait A { function f() {} }
    /// trait B { function f() {} }
    /// class C { use A, B { A::f insteadof A; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Inconsistent insteadof definition. The method f is to be used from A, but A is also on the exclude list in %s on line 4
    /// ```
    ///
    /// ```php
    /// trait A { function f() {} }
    /// class C { use A { A::g as h; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: An alias was defined for A::g but this method does not exist in %s on line 3
    /// ```
    ///
    /// ```php
    /// trait A { function f() {} }
    /// trait B { function f() {} }
    /// class C { use A, B { f as g; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: An alias was defined for method f(), which exists in both A and B. Use A::f or B::f to resolve the ambiguity in %s on line 4
    /// ```
    ///
    /// ```php
    /// trait A { function f() {} }
    /// class C { use A { g as h; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: An alias (h) was defined for method g(), but this method does not exist in %s on line 3
    /// ```
    ///
    /// ```php
    /// trait A { function f() {} }
    /// class C { use A { g as protected; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: The modifiers of the trait method g() are changed, but this method does not exist. Error in %s on line 3
    /// ```
    fn init_trait_structures(&mut self) -> Result<(), Refusal> {
        for adaptation in &self.child.adaptations {
            let TraitAdaptation::Insteadof {
                trait_name,
                method,
                excluded,
            } = adaptation
            else {
                continue;
            };
            let chosen = self.named_trait(trait_name)?;
            self.usage(&chosen)?;
            if !has_method(&chosen, &method.0) {
                return Err(Refusal::PrecedenceMissing {
                    name: text(&chosen.decl.name.0),
                    method: text(&method.0),
                });
            }
            let key = lowered(&method.0);
            for name in excluded {
                let left_out = self.named_trait(name)?;
                let at = self.usage(&left_out)?;
                if self.excluded[at].contains(&key) {
                    return Err(Refusal::ExcludedTwice {
                        method: text(&method.0),
                        name: text(&left_out.decl.name.0),
                    });
                }
                self.excluded[at].push(key.clone());
                if same_class(chosen.decl, left_out.decl) {
                    return Err(Refusal::InsteadofItself {
                        method: text(&method.0),
                        name: text(&chosen.decl.name.0),
                    });
                }
            }
        }
        for adaptation in &self.child.adaptations {
            let TraitAdaptation::As {
                method: reference,
                alias,
                ..
            } = adaptation
            else {
                continue;
            };
            let method = &reference.method.0;
            let resolved = if let Some(name) = &reference.trait_name {
                let held = self.named_trait(name)?;
                self.usage(&held)?;
                if !has_method(&held, method) {
                    return Err(Refusal::AliasMissing {
                        name: text(&held.decl.name.0),
                        method: text(method),
                    });
                }
                held.decl.name.clone()
            } else {
                let mut found: Option<&Held<'a>> = None;
                for used in self.traits.iter().flatten() {
                    if !has_method(used, method) {
                        continue;
                    }
                    if let Some(first) = found {
                        return Err(Refusal::AliasAmbiguous {
                            method: text(method),
                            first: text(&first.decl.name.0),
                            second: text(&used.decl.name.0),
                        });
                    }
                    found = Some(used);
                }
                match (found, alias) {
                    (Some(found), _) => found.decl.name.clone(),
                    (None, Some(alias)) => {
                        return Err(Refusal::AliasNowhere {
                            alias: text(&alias.0),
                            method: text(method),
                        });
                    }
                    (None, None) => {
                        return Err(Refusal::ModifiersNowhere {
                            method: text(method),
                        });
                    }
                }
            };
            self.aliases.push(resolved);
        }
        Ok(())
    }

    /// `zend_traits_copy_functions` over the method at `index` of the
    /// used trait at `at`: a copy under each alias an `as` gives it,
    /// then, unless an `insteadof` excluded it, the method under its own
    /// name with the modifier of each `as` that writes no alias. The
    /// abstract pass reads no exclude table, since php frees them after
    /// the first (`exclude` false). A copy an `as` made both private and
    /// final raises php's warning, which the link records:
    ///
    /// ```php
    /// trait A { function f() { echo "A\n"; } }
    /// trait B { function f() { echo "B\n"; } }
    /// trait T { private function p() { echo "p\n"; } }
    /// class C {
    ///     use A, B { A::f insteadof B; B::f as g; A::f as protected h; }
    ///     use T { p as final; }
    ///     function q() { $this->p(); }
    /// }
    /// $c = new C; $c->f(); $c->g(); $c->q();
    /// var_dump(get_class_methods($c));
    /// try { $c->h(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect exit=0
    /// Warning: Private methods cannot be final as they are never overridden by other classes in %s on line 5
    /// A
    /// B
    /// p
    /// array(3) {
    ///   [0]=>
    ///   string(1) "q"
    ///   [1]=>
    ///   string(1) "f"
    ///   [2]=>
    ///   string(1) "g"
    /// }
    /// Call to protected method C::h() from global scope
    /// ```
    fn copies(
        &mut self,
        at: usize,
        used: &Held<'a>,
        index: usize,
        method: &MethodDecl,
        exclude: bool,
    ) -> Vec<Copy> {
        let key = lowered(&method.function.name.0);
        let from = used.decl.name.clone();
        let body = used.body(index);
        let adapts = |alias_at: usize, reference: &TraitMethodRef| {
            self.aliases
                .get(alias_at)
                .is_some_and(|resolved| resolved.0.eq_ignore_ascii_case(&from.0))
                && reference.method.0.eq_ignore_ascii_case(&key)
        };
        let mut out = Vec::new();
        let mut warned = 0_usize;
        let as_list = self.child.adaptations.iter().filter_map(|a| match a {
            TraitAdaptation::As {
                method,
                modifier,
                alias,
            } => Some((method, modifier, alias)),
            TraitAdaptation::Insteadof { .. } => None,
        });
        for (alias_at, (reference, modifier, alias)) in as_list.clone().enumerate() {
            let Some(alias) = alias else {
                continue;
            };
            if !adapts(alias_at, reference) {
                continue;
            }
            let mut copy = method.clone();
            apply(&mut copy, *modifier);
            warned += usize::from(private_final(method, &copy, &alias.0));
            copy.function.name = alias.clone();
            out.push(Copy {
                key: lowered(&alias.0),
                method: copy,
                original: method.function.name.clone(),
                body: body.clone(),
                from: from.clone(),
            });
        }
        let left_out = exclude
            && self
                .excluded
                .get(at)
                .is_some_and(|keys| keys.contains(&key));
        if !left_out {
            let mut copy = method.clone();
            for (alias_at, (reference, modifier, alias)) in as_list.enumerate() {
                if alias.is_none() && modifier.is_some() && adapts(alias_at, reference) {
                    copy.visibility = method.visibility;
                    copy.is_final = method.is_final;
                    apply(&mut copy, *modifier);
                }
            }
            warned += usize::from(private_final(method, &copy, &method.function.name.0));
            out.push(Copy {
                key,
                method: copy,
                original: method.function.name.clone(),
                body,
                from,
            });
        }
        for _ in 0..warned {
            self.raised.push(Raised::PrivateFinal(Site {
                file: self.child.file.clone(),
                line: self.child.line,
            }));
        }
        out
    }

    /// `zend_add_trait_method` in the pass before the parent: the same
    /// body at the same visibility from a trait not yet fixed up is
    /// nothing new, the class's own method wins, two traits' methods
    /// under one name are php's collision, and anything else is taken.
    ///
    /// ```php
    /// trait T { public function hello() { return "T"; } }
    /// class C { use T; public function hello() { return "C"; } }
    /// class D extends C { use T; }
    /// echo (new C)->hello(), (new D)->hello(), "\n";
    /// ```
    /// ```expect exit=0
    /// CT
    /// ```
    ///
    /// ```php
    /// trait A { function f() { echo "A"; } }
    /// trait B { function f() { echo "B"; } }
    /// class C { use A, B; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Trait method B::f has not been applied as C::f, because of collision with A::f in %s on line 4
    /// ```
    fn add_method(&self, rows: &mut Vec<Row>, copy: Copy) -> Result<(), Refusal> {
        if let Some(existing) = rows.iter().position(|row| row.key == copy.key) {
            let row = &rows[existing];
            if row.body == copy.body
                && row.method.visibility == copy.method.visibility
                && matches!(row.scope, Scope::Trait(_))
            {
                return Ok(());
            }
            match &row.scope {
                Scope::Own => return Ok(()),
                Scope::Trait(holder) if !row.method.is_abstract => {
                    return Err(Refusal::TraitCollision(Box::new(Collision {
                        from: text(&copy.from.0),
                        method: text(&copy.original.0),
                        class: text(&self.child.name.0),
                        name: text(&copy.method.function.name.0),
                        holder: text(&holder.0),
                        held: text(&row.method.function.name.0),
                    })));
                }
                Scope::Trait(_) => {}
            }
            rows[existing] = Row {
                key: copy.key,
                method: copy.method,
                body: copy.body,
                scope: Scope::Trait(copy.from),
            };
            return Ok(());
        }
        rows.push(Row {
            key: copy.key,
            method: copy.method,
            body: copy.body,
            scope: Scope::Trait(copy.from),
        });
        Ok(())
    }

    /// `zend_do_traits_constant_binding`: each trait's constants among
    /// the class's constants after its own, one of the same name held by
    /// `do_trait_constant_check` and left where it agrees.
    ///
    /// ```php
    /// trait A { const X = 1; const Y = [1, 'a' => 2.5]; }
    /// trait B { const X = 1; const Y = [1, 'a' => 2.5]; }
    /// class C { use A, B; const X = 1; }
    /// var_dump(C::X, C::Y['a']);
    /// ```
    /// ```expect exit=0
    /// int(1)
    /// float(2.5)
    /// ```
    ///
    /// ```php
    /// trait A { const X = 1; }
    /// class C { use A; const X = 2; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: C and A define the same constant (X) in the composition of C. However, the definition differs and is considered incompatible. Class was composed in %s on line 3
    /// ```
    ///
    /// ```php
    /// trait A { const X = 1; }
    /// enum E { use A; case X; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot use trait A, because A::X conflicts with enum case E::X in %s on line 3
    /// ```
    fn bind_constants(&self) -> Result<Vec<ClassConstDecl>, Refusal> {
        let mut table = self.child.constants.clone();
        for (at, used) in self.traits.iter().enumerate() {
            let Some(used) = used else {
                continue;
            };
            for constant in &used.decl.constants {
                let Some(existing) = table.iter().find(|c| c.name == constant.name) else {
                    table.push(constant.clone());
                    continue;
                };
                let (from, name) = (text(&used.decl.name.0), text(&constant.name.0));
                if existing.is_case {
                    return Err(Refusal::TraitConstantCase {
                        from,
                        constant: name,
                        class: text(&self.child.name.0),
                    });
                }
                let compatible = existing.visibility == constant.visibility
                    && existing.is_final == constant.is_final
                    && existing.ty == constant.ty
                    && self.identical(Some(&existing.value), Some(&constant.value));
                if !compatible {
                    let first = self.first_definition(at, |held| {
                        held.decl.constants.iter().any(|c| c.name == constant.name)
                    });
                    return Err(Refusal::TraitConstant {
                        first,
                        from,
                        constant: name,
                        class: text(&self.child.name.0),
                    });
                }
            }
        }
        Ok(table)
    }

    /// `zend_do_traits_property_binding`: each trait's properties among
    /// the class's properties after its own, one of the same name compatible
    /// where its flags, its type and its default agree, and a readonly
    /// class taking no property that is not.
    ///
    /// ```php
    /// trait A { public $x = 1; public static $s = []; }
    /// trait B { public $x = 1; }
    /// class C { use A, B; public $y; }
    /// var_dump(new C);
    /// ```
    /// ```expect exit=0
    /// object(C)#1 (2) {
    ///   ["y"]=>
    ///   NULL
    ///   ["x"]=>
    ///   int(1)
    /// }
    /// ```
    ///
    /// ```php
    /// trait A { public $x = 1; }
    /// class C { use A; public $x = 2; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: C and A define the same property ($x) in the composition of C. However, the definition differs and is considered incompatible. Class was composed in %s on line 3
    /// ```
    fn bind_properties(&self) -> Result<Vec<PropertyDecl>, Refusal> {
        let mut table = self.child.properties.clone();
        for (at, used) in self.traits.iter().enumerate() {
            let Some(used) = used else {
                continue;
            };
            for property in &used.decl.properties {
                let from = text(&used.decl.name.0);
                if let Some(existing) = table.iter().find(|p| p.name == property.name) {
                    if existing.hooks.is_some() || property.hooks.is_some() {
                        let first = self.first_definition(at, |held| {
                            held.decl.properties.iter().any(|p| p.name == property.name)
                        });
                        return Err(Refusal::TraitHookedProperty {
                            first,
                            from,
                            property: text(&property.name.0),
                            class: text(&self.child.name.0),
                        });
                    }
                    let compatible = existing.visibility == property.visibility
                        && existing.is_instance == property.is_instance
                        && existing.is_readonly == property.is_readonly
                        && existing.ty == property.ty
                        && self.identical(
                            default_of(existing).as_ref(),
                            default_of(property).as_ref(),
                        );
                    if compatible {
                        continue;
                    }
                    let first = self.first_definition(at, |held| {
                        held.decl.properties.iter().any(|p| p.name == property.name)
                    });
                    return Err(Refusal::TraitProperty {
                        first,
                        from,
                        property: text(&property.name.0),
                        class: text(&self.child.name.0),
                    });
                }
                if self.child.flags.is_readonly && !property.is_readonly {
                    return Err(Refusal::ReadonlyTraitProperty {
                        class: text(&self.child.name.0),
                        from,
                        property: text(&property.name.0),
                    });
                }
                table.push(property.clone());
            }
        }
        Ok(table)
    }

    /// `find_first_constant_definition` and its property twin: the
    /// first trait before the one at `before` that has the member, and
    /// the class itself where none has, the member being its own.
    fn first_definition(&self, before: usize, has: impl Fn(&Held<'a>) -> bool) -> String {
        self.traits
            .iter()
            .take(before)
            .flatten()
            .find(|held| has(held))
            .map_or_else(|| text(&self.child.name.0), |held| text(&held.decl.name.0))
    }
}

/// An `as`'s modifier on the copy: a visibility replaces the method's,
/// `final` is added to it.
fn apply(copy: &mut MethodDecl, modifier: Option<AliasModifier>) {
    match modifier {
        Some(AliasModifier::Visibility(visibility)) => copy.visibility = visibility,
        Some(AliasModifier::Final) => copy.is_final = true,
        None => {}
    }
}

/// `zend_traits_check_private_final_inheritance`: a copy an `as` made
/// both private and final, where the trait's method was not final, and
/// is no constructor, raises php's warning.
fn private_final(original: &MethodDecl, copy: &MethodDecl, name: &[u8]) -> bool {
    !original.is_final
        && copy.visibility == Visibility::Private
        && copy.is_final
        && !name.eq_ignore_ascii_case(b"__construct")
}

/// The default a property's slot starts with: a typed property written
/// with none is uninitialized, an untyped one is `null`.
fn default_of(property: &PropertyDecl) -> Option<ConstExpr> {
    match (&property.default, &property.ty) {
        (Some(default), _) => Some(default.clone()),
        (None, Some(_)) => None,
        (None, None) => Some(ConstExpr::Null),
    }
}

impl Composer<'_, '_> {
    /// `check_trait_property_or_constant_value_compatibility` over two
    /// values after `zval_update_constant_ex` evaluated a side that is
    /// an expression: `fast_is_identical_function` on literals, element
    /// by element on arrays, and a side the caller cannot evaluate
    /// compared as written; no value at all is the same as none.
    ///
    /// ```php
    /// const VALUE = true;
    /// trait Foo { public $var = VALUE; const K = 40 + 2; }
    /// trait Bar { public $var = true; const K = 42; }
    /// class Baz { use Foo, Bar; }
    /// var_dump((new Baz)->var, Baz::K);
    /// ```
    /// ```expect
    /// bool(true)
    /// int(42)
    /// ```
    ///
    /// A bare constant written in a namespace is the namespace's, so it
    /// is the same value as the one written fully qualified (#1270):
    ///
    /// ```php
    /// namespace N { const FOO = "foo"; trait T1 { public $var = FOO; } }
    /// namespace { trait T2 { public $var = N\FOO; } class Baz { use N\T1, T2; } var_dump((new Baz)->var); }
    /// ```
    /// ```expect
    /// string(3) "foo"
    /// ```
    fn identical(&self, a: Option<&ConstExpr>, b: Option<&ConstExpr>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                let (x, y) = ((self.values)(a), (self.values)(b));
                same_value(x.as_ref().unwrap_or(a), y.as_ref().unwrap_or(b))
            }
            _ => false,
        }
    }
}

#[expect(
    clippy::float_cmp,
    reason = "`fast_is_identical_function` compares two doubles with `==`, so NAN is not identical to itself and 0.0 is to -0.0"
)]
fn same_value(a: &ConstExpr, b: &ConstExpr) -> bool {
    match (a, b) {
        (ConstExpr::Double(x), ConstExpr::Double(y)) => x == y,
        // A string named by its bytes and one a request's unit holds.
        (ConstExpr::Str(x), ConstExpr::UnitValue(y))
        | (ConstExpr::UnitValue(y), ConstExpr::Str(x)) => {
            y.value().as_str().is_some_and(|s| s.bytes() == &**x)
        }
        (ConstExpr::Array { elements: x, .. }, ConstExpr::Array { elements: y, .. }) => {
            x.len() == y.len()
                && x.iter().zip(y).all(|((kx, vx), (ky, vy))| {
                    let keys = match (kx, ky) {
                        (Some(kx), Some(ky)) => same_value(kx, ky),
                        (None, None) => true,
                        _ => false,
                    };
                    keys && same_value(vx, vy)
                })
        }
        _ => unlined(a) == unlined(b),
    }
}

/// An expression as written with every line cleared, since a node's
/// line says where it stands and never what it is: two expressions
/// written alike on two lines are written the same.
fn unlined(expr: &ConstExpr) -> ConstExpr {
    fn clear(expr: &mut ConstExpr) {
        use ConstExpr as E;
        match expr {
            E::Constant { line, .. }
            | E::ClassConstant { line, .. }
            | E::EnumCase { line, .. }
            | E::ClassName { line, .. }
            | E::FunctionCallable { line, .. }
            | E::MethodCallable { line, .. } => {
                *line = 0;
            }
            E::Unary { operand, line, .. }
            | E::Cast { operand, line, .. }
            | E::DynamicClassConstant {
                name: operand,
                line,
                ..
            } => {
                *line = 0;
                clear(operand);
            }
            E::Binary { lhs, rhs, line, .. }
            | E::Dim {
                container: lhs,
                dim: rhs,
                line,
            }
            | E::Prop {
                object: lhs,
                name: rhs,
                line,
                ..
            } => {
                *line = 0;
                clear(lhs);
                clear(rhs);
            }
            E::Array { elements, line } => {
                *line = 0;
                for (key, value) in elements {
                    if let Some(key) = key {
                        clear(key);
                    }
                    clear(value);
                }
            }
            E::New {
                arguments, line, ..
            } => {
                *line = 0;
                for (_, value) in arguments {
                    clear(value);
                }
            }
            E::Unpack(inner) => clear(inner),
            E::Conditional {
                cond,
                then,
                otherwise,
            } => {
                clear(cond);
                if let Some(then) = then {
                    clear(then);
                }
                clear(otherwise);
            }
            E::Null
            | E::Bool(_)
            | E::Long { .. }
            | E::Double(_)
            | E::Str(_)
            | E::UnitValue(_)
            | E::Closure { .. }
            | E::ScopeClass => {}
        }
    }
    let mut unlined = expr.clone();
    clear(&mut unlined);
    unlined
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_unit::decl::Type;

    #[test]
    fn a_nan_is_not_identical_to_itself_and_an_untyped_property_defaults_to_null() {
        assert!(!same_value(
            &ConstExpr::Double(f64::NAN),
            &ConstExpr::Double(f64::NAN)
        ));
        assert!(same_value(
            &ConstExpr::Double(0.0),
            &ConstExpr::Double(-0.0)
        ));
        let property = PropertyDecl {
            name: Name::from("p"),
            visibility: Visibility::Public,
            set_visibility: None,
            is_instance: true,
            is_readonly: false,
            is_final: false,
            is_promoted: false,
            is_virtual: false,
            ty: None,
            default: None,
            attributes: Vec::new(),
            doc_comment: None,
            hooks: None,
        };
        assert_eq!(default_of(&property), Some(ConstExpr::Null));
        let typed = PropertyDecl {
            ty: Some(Type::Int),
            ..property
        };
        assert_eq!(default_of(&typed), None);
    }
}
