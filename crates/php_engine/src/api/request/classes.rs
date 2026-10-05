//! The classes and functions a request declares: `EG(class_table)`
//! past the engine's, a unit's classes declared at its load or at their
//! `DECLARE_CLASS` and `DECLARE_CLASS_DELAYED`, each linked with its
//! members, hooks and traits, anonymous classes, `class_alias()`, the
//! functions a unit declares and the closures it makes, and the lookups
//! by name and number that read them all. Which classes a load bound
//! early is [`Classes`]'s, whose field no file but this one reads.

use std::sync::{Arc, OnceLock};

use php_heap::{FrameRef, Heap};
use php_unit::decl::{ClassDecl, ClassFlags, ClassKind, FunctionDecl, HookKind};
use php_unit::{FramelessEntry, Opcode, Unit};

use crate::api::call::Function;
use crate::api::raising::{Level, Thrown};
use crate::api::values::{ClassId, Key, PhpArray, PhpString, Value};
use crate::engine::Shared;
use crate::exec;

use super::{
    Declared, Deprecations, Keyed, Member, MemberAt, Request, UserFunction, lowered_key, preload,
    user_function_decl,
};

/// The classes a load bound early, whose field no file but this one
/// reads.
#[derive(Default)]
pub(super) struct Classes {
    /// The classes a unit's load bound early at their
    /// `DECLARE_CLASS_DELAYED` ([`Request::declare_classes`]), each the
    /// unit and the class's index, until that opline runs and passes
    /// over it: php's runtime definition key gone from `EG(class_table)`
    /// once `zend_accel_do_delayed_early_binding` has bound the class
    /// under its name. A load that bound nothing leaves nothing here, so
    /// the opline of a file loaded again declares, and is refused.
    bound_at_load: Vec<(Arc<Unit>, u32)>,
}

impl Request {
    /// `EG(class_table)` in its order, each row the class and, for an
    /// alias's row, the lowered name it is keyed by: the engine's
    /// classes, each native alias after its class, then the request's
    /// rows in their buckets.
    pub(crate) fn class_table(&self) -> Vec<(ClassId, Option<Vec<u8>>)> {
        let natives = self.shared.classes.len();
        let mut rows: Vec<(ClassId, Option<Vec<u8>>)> = (0u32..)
            .zip(&self.shared.classes)
            .flat_map(|(at, decl)| {
                let aliases = decl.aliases.iter();
                std::iter::once(None)
                    .chain(aliases.map(|alias| Some(alias.0.to_ascii_lowercase())))
                    .map(move |alias| (ClassId(at), alias))
            })
            .collect();
        let mut own: Vec<(preload::Bucket, ClassId, Option<Vec<u8>>)> = (natives..)
            .zip(self.declared.iter())
            .map(|(at, row)| {
                (
                    row.place,
                    ClassId(u32::try_from(at).unwrap_or(u32::MAX)),
                    None,
                )
            })
            .chain(
                self.aliases
                    .iter()
                    .map(|(place, alias, class)| (*place, *class, Some(alias.clone()))),
            )
            .collect();
        own.sort_by_key(|(place, _, _)| *place);
        rows.extend(own.into_iter().map(|(_, class, alias)| (class, alias)));
        rows
    }

    /// `do_bind_class` for every class the unit declares that php bound
    /// while the file compiled, before its main op array runs: a
    /// top-level class implementing no interface, with no parent or one
    /// the table already held, is early bound and the compiler emitted
    /// no opline for it, so the request's table takes it here. Every
    /// other class carries a `ZEND_DECLARE_CLASS` where it was written,
    /// in whichever op array of the unit that is, and is filed under its
    /// runtime definition key ([`Unit::runtime_keys`]), so it is passed
    /// over here and bound as the program serves ([`Request::bind_class`]).
    /// A name already declared is `zend_class_redeclaration_error`,
    /// `E_COMPILE_ERROR` in php's two wordings, the site of the earlier
    /// declaration named when it was a user class, and the request ends
    /// before the frame is pushed; php runs nothing of a file whose
    /// class fails to bind.
    ///
    /// A class the main op array declares with `DECLARE_CLASS_DELAYED`
    /// is tried again here, in its place among the others, as
    /// `zend_accel_do_delayed_early_binding` tries it when opcache loads
    /// a cached file: the compiler knows the engine's classes and the file's
    /// own, and a parent an earlier file of the request declared is
    /// what php's compiler, which sees the request's whole table, binds
    /// early. So a class that extends one from an include before it
    /// exists before its line runs:
    ///
    /// ```php
    /// file_put_contents('child.php', '<?php var_dump(class_exists("C", false)); class C extends P {} class Q {}');
    /// class P {}
    /// include 'child.php';
    /// echo implode(' ', array_slice(get_declared_classes(), -3)), "\n";
    /// unlink('child.php');
    /// ```
    /// ```expect
    /// bool(true)
    /// P C Q
    /// ```
    ///
    /// A unit opcache served, held or kept in its file cache, tries them
    /// once its other classes are declared, as `zend_accel_load_script`
    /// copies the cached file's classes before the delayed early binding,
    /// so a parent written below its child is there by then; what the
    /// link refuses names the child's line, the one `zend_try_early_bind`
    /// compiles at (GH-16508). Any other unit tries each in its place, as
    /// php's own compiler early binds. Served by the binary, whose
    /// program is a file opcache holds:
    ///
    /// ```php
    /// echo "never\n";
    /// class Test2 extends Test {}
    /// abstract class Test { abstract function foo(); }
    /// ```
    /// ```expect exit=255 regime=jit-off process
    /// Fatal error: Class Test2 contains 1 abstract method and must therefore be declared abstract or implement the remaining method (Test::foo) in %s on line 3
    /// ```
    ///
    /// That is a file opcache compiles with `ZEND_COMPILE_DELAYED_BINDING`
    /// and keeps: not one it keeps out, by `opcache.blacklist_filename` or
    /// `opcache.max_file_size`, nor one written within
    /// `opcache.file_update_protection`, which `accelerator_orig_compile_file`
    /// compiles as php without opcache does. Served by the binary, since
    /// the blacklist is read at startup:
    ///
    /// ```php
    /// $dir = realpath(sys_get_temp_dir()) . '/delayed' . getmypid(); @mkdir($dir);
    /// file_put_contents("$dir/c.php", '<?php var_dump(class_exists("C", false)); class C extends P {} class P {}');
    /// touch("$dir/c.php", time() - 10);
    /// file_put_contents("$dir/list", "$dir/c.php\n");
    /// foreach (["-d opcache.blacklist_filename=$dir/list", '', '-d opcache.file_update_protection=60'] as $ini) {
    ///     echo shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 -d opcache.jit=off $ini $dir/c.php");
    /// }
    /// exec('rm -rf ' . escapeshellarg($dir));
    /// ```
    /// ```expect regime=jit-off process
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// ```
    ///
    /// A file compiled while the request already holds a class of its
    /// name binds none early, as php's compiler, which sees the
    /// request's table, leaves it to its opline (GH-8846): what stands
    /// before the declaration serves, and the opline raises the
    /// redeclaration.
    ///
    /// ```php
    /// class Foo {}
    /// file_put_contents(__DIR__ . '/again.inc', "<?php\necho \"before\\n\";\nclass Foo {}\n");
    /// register_shutdown_function(fn () => unlink(__DIR__ . '/again.inc'));
    /// include __DIR__ . '/again.inc';
    /// ```
    /// ```expect exit=255
    /// before
    ///
    /// Fatal error: Cannot redeclare class Foo (previously declared in %s:2) in %s on line 3
    /// ```
    pub(crate) fn declare_classes(&mut self, unit: &Arc<Unit>) -> bool {
        let delayed: Vec<Vec<u8>> = unit
            .main
            .oplines
            .iter()
            .filter(|op| op.opcode == Opcode::DeclareClassDelayed)
            .filter_map(|op| {
                let key = usize::try_from(op.op1).ok()?.checked_add(1)?;
                unit.main
                    .literals
                    .get(key)?
                    .as_str()
                    .map(|key| key.bytes().to_vec())
            })
            .collect();
        let accelerated = self.preloading.is_some()
            || (self.accel_loaded.take()).is_some_and(|loaded| Arc::ptr_eq(&loaded, unit));
        let mut tried = Vec::new();
        for (at, decl) in unit.classes.iter().enumerate() {
            // A saved script of the preload, whose classes the preload
            // holds, declares none of them again (`preload_remove_declares`).
            if self.preloaded_class(unit, at) {
                continue;
            }
            let key = unit
                .runtime_keys
                .iter()
                .find(|(_, class)| usize::try_from(*class).ok() == Some(at));
            if let Some((key, _)) = key {
                self.buckets
                    .reserve(unit, u32::try_from(at).unwrap_or(u32::MAX));
                if decl.flags.is_anonymous {
                    if !self.link_unbound(unit, at, decl) {
                        return false;
                    }
                    continue;
                }
                if delayed.iter().any(|delayed| **delayed == *key.0) {
                    if accelerated {
                        tried.push((at, decl));
                    } else if !self.bind_at_load(unit, at, decl) {
                        return false;
                    }
                }
                continue;
            }
            if let Some(existing) = self.class_named(&decl.name.0) {
                let message = self.redeclaration(existing);
                let file = decl.file.0.to_vec();
                self.end_in_compile_time(Level::CompileError, &message, &file, decl.line);
                return false;
            }
            let Ok(at) = u32::try_from(at) else {
                return false;
            };
            self.bind_declared(unit, at, None);
        }
        tried
            .into_iter()
            .all(|(at, decl)| self.bind_at_load(unit, at, decl))
    }

    /// One entry of `zend_accel_do_delayed_early_binding`: the class at
    /// `at` of `unit` bound now where its name is free and its parent,
    /// if it names one, is already declared, looked up without
    /// autoloading, then linked as the early bind links
    /// ([`Request::link_declared`] under `early`), and recorded so its
    /// opline passes over it. A link that cannot resolve yet leaves the
    /// class to the opline; one that is refused ends the request before
    /// the file runs, as php's compile-time early bind would. False when
    /// the request ended.
    fn bind_at_load(&mut self, unit: &Arc<Unit>, at: usize, decl: &ClassDecl) -> bool {
        if self.class_named(&decl.name.0).is_some() {
            return true;
        }
        if let Some(parent) = &decl.parent
            && self.class_named(&parent.0).is_none()
        {
            return true;
        }
        match self.link_declared(unit, at, decl, true) {
            Ok(true) => {
                let at = u32::try_from(at).unwrap_or(u32::MAX);
                self.classes.bound_at_load.push((Arc::clone(unit), at));
                true
            }
            Ok(false) => true,
            Err(_) => false,
        }
    }

    /// `zend_compile_class_decl`'s `link_unbound` for the anonymous class
    /// at `at` of `unit`: one with no parent, no interface and no trait
    /// php links while the file compiles and holds in `CG(class_table)`
    /// from then on, so it is linked and bound as the unit loads, and
    /// its `DECLARE_ANON_CLASS` finds it by name
    /// ([`Request::declare_anonymous`]); any other is linked by the
    /// opline. A name already bound is the same unit loaded again, whose
    /// class stands. False when the link ended the request.
    ///
    /// ```php
    /// $before = array_map(fn($c) => strstr($c, "\0", true) ?: $c, array_slice(get_declared_classes(), -2));
    /// $o = new class {};
    /// class Q {}
    /// var_dump($before);
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   string(15) "class@anonymous"
    ///   [1]=>
    ///   string(1) "Q"
    /// }
    /// ```
    fn link_unbound(&mut self, unit: &Arc<Unit>, at: usize, decl: &ClassDecl) -> bool {
        let unbound = decl.parent.is_none() && decl.interfaces.is_empty() && decl.traits.is_empty();
        if !unbound || self.class_named(&decl.name.0).is_some() {
            return true;
        }
        self.link_declared(unit, at, decl, true).is_ok()
    }

    /// `ZEND_DECLARE_CLASS_DELAYED` (`Z/Zend/zend_vm_def.h:7968`):
    /// nothing where the unit's load bound the class under the opline's
    /// runtime definition key ([`Request::bind_at_load`]), and
    /// [`Request::bind_class`] where it did not, the key still standing.
    pub(crate) fn bind_delayed(&mut self, unit: &Arc<Unit>, key: &[u8]) -> Result<(), Thrown> {
        let at = unit
            .runtime_keys
            .iter()
            .find(|(filed, _)| *filed.0 == *key)
            .map(|(_, at)| *at);
        if let Some(bound) = self
            .classes
            .bound_at_load
            .iter()
            .position(|(held, class)| Arc::ptr_eq(held, unit) && Some(*class) == at)
        {
            self.classes.bound_at_load.swap_remove(bound);
            return Ok(());
        }
        self.bind_class(unit, key)
    }

    /// `zend_class_redeclaration_error`'s two wordings, which name the
    /// earlier declaration as it was declared, what it is and its own
    /// name, and its site when it was a user class.
    ///
    /// ```php
    /// class stdclass {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot redeclare class stdClass in %s on line 2
    /// ```
    fn redeclaration(&self, existing: ClassId) -> String {
        match self.class_decl(existing) {
            Some(earlier) => redeclaration(earlier),
            None => String::from("InternalError: a class redeclared over none"),
        }
    }

    /// The class at `at` of `unit` into the request's table, with its
    /// methods into the function table beside it, which is the whole of
    /// what binding a class is here: the parent chain is walked from the
    /// declaration on every lookup, so nothing is copied. It stands in
    /// the bucket its unit's load took for it, where php's runtime
    /// definition key stood, or in the next:
    ///
    /// ```php
    /// class A {}
    /// if (true) { class B {} }
    /// class C implements Countable { function count(): int { return 0; } }
    /// class D {}
    /// function f() { class F {} }
    /// f();
    /// class_alias('A', 'AA');
    /// echo implode(",", array_slice(get_declared_classes(), -6)), "\n";
    /// ```
    /// ```expect
    /// A,B,C,D,F,aa
    /// ```
    pub(super) fn bind_declared(
        &mut self,
        unit: &Arc<Unit>,
        at: u32,
        composed: Option<php_unit::link::Composed>,
    ) {
        let class = self
            .shared
            .classes
            .len()
            .saturating_add(self.declared.len());
        let class = ClassId(u32::try_from(class).unwrap_or(u32::MAX));
        let place = self.buckets.claim(unit, at);
        self.declared.push(Declared {
            unit: Arc::clone(unit),
            at,
            composed: composed.map(Box::new),
            place,
            placed: OnceLock::new(),
        });
        self.bind_members(class);
    }

    /// The members of `class`, bound, into the function table beside it:
    /// an enum's own `cases`, `from` and `tryFrom`, each method, and
    /// each hook of the class's own properties.
    pub(super) fn bind_members(&mut self, class: ClassId) {
        let Some(decl) = self.class_decl(class) else {
            return;
        };
        // `zend_enum_register_funcs` at the link: the enum's own
        // `cases`, `from` and `tryFrom`, which the compiler declared
        // after its methods and which no body of the unit serves.
        let mut enum_functions = Vec::new();
        if decl.kind == ClassKind::Enum {
            for method in &decl.methods {
                let lowered = method.function.name.0.to_ascii_lowercase();
                if let Some(method) = crate::enums::EnumMethod::of(&lowered, decl.backing) {
                    enum_functions.push((class, method));
                }
            }
        }
        self.enum_functions.extend(enum_functions);
        let methods = self.class_decl(class).map_or(0, |decl| decl.methods.len());
        for method in 0..methods {
            let method = u32::try_from(method).unwrap_or(u32::MAX);
            let Some((unit, body)) = self.method_body(class, method) else {
                continue;
            };
            let flags = self
                .class_decl(class)
                .and_then(|decl| decl.methods.get(usize::try_from(method).ok()?))
                .map(|method| crate::attributes::Flags::of(&method.function.attributes))
                .unwrap_or_default();
            self.user_functions.push(UserFunction {
                unit,
                body,
                member: Some(Member {
                    class,
                    at: MemberAt::Method(method),
                }),
                flags,
            });
        }
        // Each hook of the class's own properties, `prop_info->hooks`,
        // a function of its own that php's function table never lists.
        let hooks: Vec<(u32, HookKind)> = self.class_decl(class).map_or_else(Vec::new, |decl| {
            decl.properties
                .iter()
                .enumerate()
                .flat_map(|(at, property)| {
                    let at = u32::try_from(at).unwrap_or(u32::MAX);
                    HookKind::ALL.into_iter().filter_map(move |kind| {
                        property
                            .hooks
                            .as_ref()
                            .and_then(|hooks| hooks.of(kind))
                            .map(|_| (at, kind))
                    })
                })
                .collect()
        });
        for (property, kind) in hooks {
            let Some((unit, body)) = self.hook_body(class, property, kind) else {
                continue;
            };
            let flags = self
                .class_decl(class)
                .and_then(|decl| decl.properties.get(usize::try_from(property).ok()?))
                .and_then(|property| property.hooks.as_ref()?.of(kind))
                .map(|hook| crate::attributes::Flags::of(&hook.function.attributes))
                .unwrap_or_default();
            self.user_functions.push(UserFunction {
                unit,
                body,
                member: Some(Member {
                    class,
                    at: MemberAt::Hook { property, kind },
                }),
                flags,
            });
        }
    }

    /// Where the op array of a hook of the property at `property` of a
    /// class the request declared is: its own unit's body, or for a
    /// property a trait gave the class the body in the trait's unit,
    /// which php shares between the copies as it shares a method's
    /// (`zend_do_traits_property_binding`).
    fn hook_body(
        &self,
        class: ClassId,
        property: u32,
        kind: HookKind,
    ) -> Option<(Arc<Unit>, exec::Body)> {
        let declared = self.declared.get(
            usize::try_from(class.0)
                .ok()?
                .checked_sub(self.shared.classes.len())?,
        )?;
        let decl = declared.decl()?;
        let name = &decl.properties.get(usize::try_from(property).ok()?)?.name;
        let own = declared
            .unit
            .classes
            .get(usize::try_from(declared.at).ok()?)?;
        let (from, at) = match own.properties.iter().position(|p| p.name == *name) {
            Some(at) => (declared, at),
            None => decl.traits.iter().find_map(|trait_name| {
                let holder = self.class_named(&trait_name.0)?;
                let from = self.declared.get(
                    usize::try_from(holder.0)
                        .ok()?
                        .checked_sub(self.shared.classes.len())?,
                )?;
                let at = from
                    .decl()?
                    .properties
                    .iter()
                    .position(|p| p.name == *name && p.hooks.is_some())?;
                Some((from, at))
            })?,
        };
        Some((
            Arc::clone(&from.unit),
            exec::Body::Hook {
                class: from.at,
                property: u32::try_from(at).ok()?,
                kind,
            },
        ))
    }

    /// Where the op array of the method at `method` of a class the
    /// request declared is: its own unit's body, or for a method a trait
    /// gave it the body in the unit of the class the composition names,
    /// which the table held when the class linked. The copy runs the
    /// trait's op array with the class as its scope, as
    /// `zend_fixup_trait_method` leaves it, so `self`, `static`, a
    /// closure made in it and a `static` property are the using class's,
    /// each class with its own:
    ///
    /// ```php
    /// trait T {
    ///     public static $n = 0;
    ///     static function inc() { return ++self::$n; }
    ///     function f() { return function () { return [static::class, self::class, __CLASS__]; }; }
    /// }
    /// class A { use T; }
    /// class B { use T; }
    /// class D extends A {}
    /// A::inc(); A::inc(); B::inc();
    /// var_dump(A::$n, B::$n, ((new D)->f())());
    /// ```
    /// ```expect
    /// int(2)
    /// int(1)
    /// array(3) {
    ///   [0]=>
    ///   string(1) "D"
    ///   [1]=>
    ///   string(1) "A"
    ///   [2]=>
    ///   string(1) "A"
    /// }
    /// ```
    fn method_body(&self, class: ClassId, method: u32) -> Option<(Arc<Unit>, exec::Body)> {
        let declared = self.declared.get(
            usize::try_from(class.0)
                .ok()?
                .checked_sub(self.shared.classes.len())?,
        )?;
        let Some(composed) = &declared.composed else {
            return Some((
                Arc::clone(&declared.unit),
                exec::Body::Method {
                    class: declared.at,
                    method,
                },
            ));
        };
        let body = composed.bodies.get(usize::try_from(method).ok()?)?;
        let own = composed.decl.name.0.eq_ignore_ascii_case(&body.class.0);
        let from = if own {
            declared
        } else {
            let holder = self.class_named(&body.class.0)?;
            self.declared.get(
                usize::try_from(holder.0)
                    .ok()?
                    .checked_sub(self.shared.classes.len())?,
            )?
        };
        Some((
            Arc::clone(&from.unit),
            exec::Body::Method {
                class: from.at,
                method: body.method,
            },
        ))
    }

    /// `do_bind_class` at a `ZEND_DECLARE_CLASS`: the declaration the
    /// unit files under the opline's runtime definition key
    /// ([`Unit::runtime_keys`]), refused where its name is already
    /// declared as `zend_class_redeclaration_error` refuses it, at
    /// `E_COMPILE_ERROR` though the program is running, which is also
    /// what the same opline running a second time meets; then
    /// `zend_do_link_class`, the one link the early bind makes too
    /// (`php_unit::link::link`), with the parent fetched first, then
    /// each trait, then each interface, all of them before any is
    /// checked, which is the order php's messages come in. A name
    /// nothing declares, once autoloading has been asked, is php's
    /// `Error`, `Class "P" not found` for a parent, `Trait "T" not
    /// found` for a trait and `Interface "I" not found` for an
    /// interface, and a class used as a trait is refused as php's
    /// `ZEND_FETCH_CLASS_TRAIT` refuses it.
    ///
    /// A class declared in a function body is bound each time the
    /// function runs, and the second time is the redeclaration, at the
    /// level `error_get_last()` reads as 64:
    ///
    /// ```php
    /// register_shutdown_function(function () { var_dump(error_get_last()['type']); });
    /// function f() {
    ///     class G {}
    ///     echo "declared\n";
    /// }
    /// f();
    /// f();
    /// ```
    /// ```expect exit=255
    /// declared
    ///
    /// Fatal error: Cannot redeclare class G (previously declared in %s:4) in %s on line 4
    /// int(64)
    /// ```
    ///
    /// A second top-level declaration of a name the file bound is not
    /// bound early, and its opline raises the redeclaration where it
    /// stands, after what ran before it:
    ///
    /// ```php
    /// echo "a\n";
    /// class A {}
    /// class A {}
    /// ```
    /// ```expect exit=255
    /// a
    ///
    /// Fatal error: Cannot redeclare class A (previously declared in %s:3) in %s on line 4
    /// ```
    ///
    /// ```php
    /// class C { use Nope; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Trait "Nope" not found in %s:2
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// class N {}
    /// class C { use N; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: C cannot use N - it is not a trait in %s:3
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 3
    /// ```
    ///
    /// A class that uses no trait is linked without a copy: an inherited
    /// constant, property or method is found by walking `parent` from
    /// the declaration, so what this adds is the class's own row and its
    /// methods. One that uses traits is the class composed with them
    /// ([`php_unit::link::Composed`]), which stands in the table in its
    /// place.
    ///
    /// The bind stands where the class was written, so what it refuses
    /// is refused after what the file has already printed, which is
    /// what tells this apart from the early bind:
    ///
    /// ```php
    /// echo "before\n";
    /// class C implements Nope {}
    /// ```
    /// ```expect exit=255
    /// before
    ///
    /// Fatal error: Uncaught Error: Interface "Nope" not found in %s:3
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 3
    /// ```
    ///
    /// Then the class is linked (`zend_do_link_class`) by the checks
    /// `zend_inheritance.c` makes, [`php_unit::link`], which the early
    /// bind runs too: a class implementing an interface is never early
    /// bound, so what linking refuses about one is refused here, after
    /// what the file printed, at the site php names, and a deprecation
    /// a tentative return raises is raised here before the class runs.
    ///
    /// ```php
    /// interface I { const X = 1; }
    /// interface J { const X = 2; }
    /// class C implements I, J {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C inherits both I::X and J::X, which is ambiguous in %s on line 4
    /// ```
    ///
    /// ```php
    /// interface I { const X = 1; }
    /// class C implements I { protected const X = 2; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Access level to C::X must be public (as in interface I) in %s on line 3
    /// ```
    ///
    /// ```php
    /// class C implements Throwable {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C cannot implement interface Throwable, extend Exception or Error instead in %s on line 2
    /// ```
    ///
    /// ```php
    /// class C implements UnitEnum {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Non-enum class C cannot implement interface UnitEnum in %s on line 2
    /// ```
    ///
    /// ```php
    /// class Foo implements UnitEnum { public static function cases(): array { return []; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Non-enum class Foo cannot implement interface UnitEnum in %s on line 2
    /// ```
    ///
    /// ```php
    /// enum Foo implements Throwable {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Enum Foo cannot implement interface Throwable in %s on line 2
    /// ```
    ///
    /// ```php
    /// enum E implements BackedEnum {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Non-backed enum E cannot implement interface BackedEnum in %s on line 2
    /// ```
    ///
    /// An interface's methods are held before what the interface itself
    /// guards (`do_interface_implementation`), and the parent before the
    /// interface list, of which a class is refused before a repeat:
    ///
    /// ```php
    /// class C implements Throwable { function getMessage(): int {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Declaration of C::getMessage(): int must be compatible with Throwable::getMessage(): string in %s on line 2
    /// ```
    ///
    /// ```php
    /// class P {}
    /// class C extends Closure implements P {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C cannot extend final class Closure in %s on line 3
    /// ```
    ///
    /// ```php
    /// class P {}
    /// interface I {}
    /// class C implements I, P, I {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: C cannot implement P - it is not an interface in %s on line 4
    /// ```
    ///
    /// ```php
    /// class P {}
    /// class C implements P {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: C cannot implement P - it is not an interface in %s on line 3
    /// ```
    ///
    /// ```php
    /// class C implements ArrayAccess {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C contains 4 abstract methods and must therefore be declared abstract or implement the remaining methods (ArrayAccess::offsetExists, ArrayAccess::offsetGet, ArrayAccess::offsetSet, ...) in %s on line 2
    /// ```
    ///
    /// ```php
    /// echo "before\n";
    /// class C implements ArrayAccess {
    ///     function offsetExists($o): bool { return true; }
    ///     function offsetGet($o): mixed { return 1; }
    ///     function offsetSet(): void {}
    ///     function offsetUnset($o): void {}
    /// }
    /// ```
    /// ```expect exit=255
    /// before
    ///
    /// Fatal error: Declaration of C::offsetSet(): void must be compatible with ArrayAccess::offsetSet(mixed $offset, mixed $value): void in %s on line 6
    /// ```
    ///
    /// ```php
    /// class A { function f(): X {} }
    /// class B extends A implements Stringable {
    ///     function __toString(): string { return ""; }
    ///     function f(): Y {}
    /// }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Could not check compatibility between B::f(): Y and A::f(): X, because class Y is not available in %s on line 5
    /// ```
    ///
    /// What php takes, it takes: an override whose parameter is wider
    /// and whose return is narrower, and one of a tentative return that
    /// declares none, which is php's deprecation and links:
    ///
    /// ```php
    /// class C implements ArrayAccess {
    ///     function offsetExists(mixed $o): bool { return true; }
    ///     function offsetGet($o) { return "got $o"; }
    ///     function offsetSet($o, $v): void {}
    ///     function offsetUnset($o): void {}
    /// }
    /// echo (new C)["k"], "\n";
    /// ```
    /// ```expect exit=0
    /// Deprecated: Return type of C::offsetGet($o) should either be compatible with ArrayAccess::offsetGet(mixed $offset): mixed, or the #[\ReturnTypeWillChange] attribute should be used to temporarily suppress the notice in %s on line 4
    /// got k
    /// ```
    ///
    /// `Traversable` itself is php's `E_CORE_ERROR`, which names no site:
    ///
    /// ```php
    /// class C implements Traversable {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C must implement interface Traversable as part of either Iterator or IteratorAggregate in Unknown on line 0
    /// ```
    ///
    /// An interface list naming one interface twice is refused in the
    /// words for what the declaration is:
    ///
    /// ```php
    /// interface I {}
    /// interface J extends I, I {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Interface J cannot implement previously implemented interface I in %s on line 3
    /// ```
    pub(crate) fn bind_class(&mut self, unit: &Arc<Unit>, key: &[u8]) -> Result<(), Thrown> {
        let Some(at) = unit
            .runtime_keys
            .iter()
            .find(|(filed, _)| *filed.0 == *key)
            .and_then(|(_, at)| usize::try_from(*at).ok())
        else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a DECLARE_CLASS carries a key the unit files no class under",
            ));
        };
        // `preload_remove_declares` made the opline of a class the
        // preload holds a `NOP`.
        if self.preloaded_class(unit, at) {
            return Ok(());
        }
        let Some(decl) = unit.classes.get(at).cloned() else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a DECLARE_CLASS carries a key the unit files no class under",
            ));
        };
        // `zend_bind_class_in_slot`'s failed `zend_hash_set_bucket_key`,
        // or `do_bind_class` finding its key gone because the opline ran
        // before: `E_COMPILE_ERROR` at the site that runs it either way.
        let existing = self
            .class_named(&decl.name.0)
            .and_then(|existing| self.class_decl(existing))
            .or_else(|| self.unlinked_decl(&decl.name.0));
        if let Some(existing) = existing {
            let message = redeclaration(existing);
            let file = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
            let line = self.current_line();
            self.end_in_compile_time(Level::CompileError, &message, &file, line);
            return Err(Thrown::of(None));
        }
        let Ok(slot) = u32::try_from(at) else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a unit declares more classes than can be numbered",
            ));
        };
        self.link_begun(unit, slot);
        let name = &decl.name.0;
        if let Some(parent) = &decl.parent
            && !self.nearly_linked(&parent.0)
            && let Err(thrown) = self.fetch_to_link(parent, "Class")
        {
            return Err(self.link_abandoned(name, thrown, true));
        }
        for used in &decl.traits {
            let found = match self.fetch_to_link(used, "Trait") {
                Ok(found) => found,
                Err(thrown) => return Err(self.link_abandoned(name, thrown, false)),
            };
            if self.class_decl(found).map(|t| t.kind) != Some(ClassKind::Trait) {
                let message = format!(
                    "{} cannot use {} - it is not a trait",
                    String::from_utf8_lossy(name),
                    self.class_name(found)
                );
                let thrown = crate::api::raising::throw_error(self, "Error", &message);
                return Err(self.link_abandoned(name, thrown, false));
            }
            if let Err(thrown) = self.deprecated_trait(found, name) {
                return Err(self.link_abandoned(name, thrown, false));
            }
        }
        for interface in &decl.interfaces {
            if !self.nearly_linked(&interface.0)
                && let Err(thrown) = self.fetch_to_link(interface, "Interface")
            {
                return Err(self.link_abandoned(name, thrown, true));
            }
        }
        self.link_declared(unit, at, &decl, false).map(|_| ())
    }

    /// The class of a `ZEND_DECLARE_ANON_CLASS`: the anonymous class the
    /// unit files under `key`, its lowered name. php's compiler put it
    /// in `CG(class_table)` under that name, and the opline links it the
    /// first time it finds it unlinked (`zend_do_link_class`), which
    /// here is [`Request::bind_class`]; after that, and where the unit's
    /// load linked it ([`Request::declare_classes`]), the name finds the
    /// class, so one declaration is one class however often its opline
    /// is reached.
    pub(crate) fn declare_anonymous(
        &mut self,
        unit: &Arc<Unit>,
        key: &[u8],
    ) -> Result<ClassId, Thrown> {
        let name = unit
            .runtime_keys
            .iter()
            .find(|(filed, _)| *filed.0 == *key)
            .and_then(|(_, at)| unit.classes.get(usize::try_from(*at).ok()?))
            .map(|decl| decl.name.0.to_vec());
        let Some(name) = name else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a DECLARE_ANON_CLASS names no class the unit files",
            ));
        };
        if let Some(class) = self.class_named(&name) {
            return Ok(class);
        }
        self.bind_class(unit, key)?;
        self.class_named(&name).ok_or_else(|| {
            crate::api::raising::fatal(
                self,
                "InternalError: an anonymous class its declaration did not bind",
            )
        })
    }

    /// `zend_do_link_class` over the class at `at` of `unit`, whose
    /// parent, traits and interfaces the table already holds: the link
    /// both the early bind and a `DECLARE_CLASS` make
    /// (`php_unit::link::link`), its diagnostics raised at their sites,
    /// a refusal the end of the request at its level, and the linked
    /// class bound. A link that needs a class the table does not hold
    /// yet is php's `INHERITANCE_UNRESOLVED`: under `early`, the answer
    /// `zend_can_early_bind` gives before anything is raised or bound,
    /// so the class is left for its opline and the answer is false;
    /// otherwise the refusal it names.
    fn link_declared(
        &mut self,
        unit: &Arc<Unit>,
        at: usize,
        decl: &ClassDecl,
        early: bool,
    ) -> Result<bool, Thrown> {
        let linking = self.link_against_table(decl);
        if early && !linking.obligations.is_empty() {
            return Ok(false);
        }
        let Ok(at) = u32::try_from(at) else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a unit declares more classes than can be numbered",
            ));
        };
        self.link_settled(unit, at, linking)?;
        Ok(true)
    }

    /// `zend_use_of_deprecated_trait` (`Z/Zend/zend_execute.c:2012`), as
    /// `zend_do_link_class` raises it for each trait it fetches that the
    /// `#[\Deprecated]` attribute marks: `Trait T used by C is
    /// deprecated`, with the attribute's `since` and `message`, at
    /// `E_USER_DEPRECATED` for a program's trait. A handler that throws
    /// stops the link.
    ///
    /// ```php
    /// #[\Deprecated("use U", "2.0")]
    /// trait T {}
    /// class C { use T; }
    /// class D extends C {}
    /// echo "done\n";
    /// ```
    /// ```expect
    ///
    /// Deprecated: Trait T used by C is deprecated since 2.0, use U in %s on line 4
    /// done
    /// ```
    fn deprecated_trait(&mut self, found: ClassId, used_by: &[u8]) -> Result<(), Thrown> {
        let attributes = match self.class_decl(found) {
            Some(decl) if crate::attributes::Flags::of(&decl.attributes).deprecated => {
                decl.attributes.clone()
            }
            _ => return Ok(()),
        };
        let suffix = crate::attributes::deprecation_suffix(self, &attributes, Some(found))?;
        let level = if self.native_class(found) {
            Level::Deprecated
        } else {
            Level::UserDeprecated
        };
        let message = format!(
            "Trait {} used by {} is deprecated{suffix}",
            self.class_name(found),
            String::from_utf8_lossy(used_by)
        );
        crate::api::raising::raise(self, level, &message);
        if self.is_pending() {
            return Err(Thrown::of(None));
        }
        Ok(())
    }

    /// A class as the link reads the table: its declaration, and the
    /// bodies of its methods where it was composed with traits.
    pub(super) fn held_class(&self, name: &[u8]) -> Option<php_unit::link::Held<'_>> {
        let class = self.class_named(name)?;
        let decl = self.class_decl(class)?;
        let bodies = usize::try_from(class.0)
            .ok()
            .and_then(|at| at.checked_sub(self.shared.classes.len()))
            .and_then(|at| self.declared.get(at))
            .and_then(|declared| declared.composed.as_ref())
            .map(|composed| composed.bodies.as_slice());
        Some(php_unit::link::Held {
            decl,
            bodies,
            linked: true,
        })
    }

    /// `zend_fetch_class_by_name` under `ZEND_FETCH_CLASS_EXCEPTION`, as
    /// `zend_do_link_class` fetches the parent and each interface before
    /// any check is made of them: the class of that name, or the one
    /// autoloading it declared, and php's `Error` where neither is,
    /// `what` naming the class a parent and the interface an interface.
    fn fetch_to_link(
        &mut self,
        name: &php_unit::decl::Name,
        what: &str,
    ) -> Result<ClassId, Thrown> {
        let found = match self.class_named(&name.0) {
            Some(found) => Some(found),
            None => crate::api::call::autoload(self, &name.0)?,
        };
        found.ok_or_else(|| {
            crate::api::raising::throw_error(
                self,
                "Error",
                &format!("{what} \"{}\" not found", String::from_utf8_lossy(&name.0)),
            )
        })
    }

    /// `zend_hash_find(EG(function_table), lcname)`: the native of that
    /// name, or the user function a unit of this request declared.
    pub(crate) fn function_named(&self, name: &[u8]) -> Option<Function> {
        let wanted = name.to_ascii_lowercase();
        if let Some(i) = self
            .shared
            .functions
            .iter()
            .position(|decl| decl.name.0.eq_ignore_ascii_case(&wanted))
        {
            return u32::try_from(i).ok().map(Function);
        }
        let position = self.user_functions.iter().position(|user| {
            user_function_decl(user).is_some_and(|decl| decl.name.0.to_ascii_lowercase() == wanted)
        })?;
        self.user_function_id(position)
    }

    /// `zend_hash_add_ptr(CG(function_table), …)` for every function the
    /// unit declares, before its main op array runs: php binds a
    /// top-level function at compile time and the compiler emitted no
    /// opline for it, so the request's table takes it here. The compiler
    /// refused a name the natives have or the file declares twice, so
    /// nothing collides in a request that serves one unit.
    /// `do_bind_function` at a `ZEND_DECLARE_FUNCTION`
    /// (`Z/Zend/zend_compile.c:1287`): the op array the opline's `op2`
    /// found among the frame's `dynamic_func_defs` into the request's
    /// function table under the lowered name in `op1`, which is
    /// `zend_hash_add_ptr(EG(function_table), lcname, func)`. A name the
    /// table already holds is `do_bind_function_error` at `E_ERROR`,
    /// naming the new declaration and, where the earlier one was a user
    /// function with a body, its file and its first line.
    pub(crate) fn bind_function(
        &mut self,
        unit: &Arc<Unit>,
        at: u32,
        lowered: &[u8],
    ) -> Result<(), Thrown> {
        let Some((decl, _)) = unit.dynamic.get(usize::try_from(at).unwrap_or(usize::MAX)) else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a DECLARE_FUNCTION naming a definition the unit has not",
            ));
        };
        // `preload_remove_declares` made the opline of a function the
        // preload holds a `NOP`.
        if self.preloaded_function(unit, exec::Body::Dynamic(at)) {
            return Ok(());
        }
        let name = String::from_utf8_lossy(&decl.name.0).into_owned();
        if let Some(earlier) = self.function_named(lowered) {
            let site = self
                .user_function(earlier)
                .and_then(user_function_decl)
                .map(|decl| {
                    format!(
                        " (previously declared in {}:{})",
                        String::from_utf8_lossy(&decl.file.0),
                        decl.line
                    )
                })
                .unwrap_or_default();
            let message = format!("Cannot redeclare function {name}(){site}");
            return Err(crate::api::raising::fatal(self, &message));
        }
        self.user_functions.push(UserFunction {
            unit: Arc::clone(unit),
            body: exec::Body::Dynamic(at),
            member: None,
            flags: dynamic_flags(unit, at),
        });
        Ok(())
    }

    /// The `Function` a closure's op array is reached by, made where
    /// the request has not named it yet: php copies the whole
    /// `zend_function` into every `Closure` it creates
    /// (`zend_create_closure_ex`'s `memcpy`), and an op array here is
    /// immutable and shared, so one name stands for the declaration
    /// and what differs between two closures of it — the receiver, the
    /// scope and the bound variables — is the object's payload. The
    /// name is no entry of the function table under a callable name: a
    /// closure is reached through its object and never by name.
    pub(crate) fn closure_function(&mut self, unit: &Arc<Unit>, at: u32) -> Option<Function> {
        unit.dynamic.get(usize::try_from(at).ok()?)?;
        let body = exec::Body::Dynamic(at);
        if let Some(position) = self
            .user_functions
            .iter()
            .position(|user| user.body == body && Arc::ptr_eq(&user.unit, unit))
        {
            return self.user_function_id(position);
        }
        self.user_functions.push(UserFunction {
            unit: Arc::clone(unit),
            body,
            member: None,
            flags: dynamic_flags(unit, at),
        });
        self.user_function_id(self.user_functions.len() - 1)
    }

    /// `EX(func)->common.scope` of a frame, the class whose body the op
    /// array it serves was written in: what `zend_create_closure` is
    /// handed as the scope of a closure declared there, and none in a
    /// function or the main op array. Read off the frame and not off
    /// [`Request::executed_scope`], which answers for the native being
    /// served where one is. A closure's frame serves the closure's own
    /// copy of its function, whose scope is the one the closure was
    /// made or bound in, which `Closure::bind` and `Closure::call` move.
    pub(crate) fn frame_scope(&self, frame: FrameRef) -> Option<ClassId> {
        let state = self.vm.at(frame)?;
        if state.closure.is_some() {
            return self.closure_scope(state.closure);
        }
        match state.function {
            Some(function) => self.method_class(function),
            None => state.scope,
        }
    }

    /// The classes a `self`, a `parent` and a `static` in a type checked
    /// in `frame` name: the frame's scope and its called scope, which
    /// is what `zend_check_type_slow` resolves them against for an
    /// argument's `RECV` and for a return.
    pub(crate) fn frame_scopes(&self, frame: FrameRef) -> crate::api::coerce::Scopes {
        let own = self.frame_scope(frame);
        let called = self.vm.at(frame).and_then(|state| state.called_scope);
        crate::api::coerce::Scopes {
            own,
            called: called.or(own),
            callables: Deprecations::Raised,
        }
    }

    /// `zend_hash_add_ptr(CG(function_table), …)` for every function the
    /// unit declares at its top level, before its main op array runs:
    /// php binds one at compile time and the compiler emitted no opline
    /// for it, so the request's table takes it here. The compiler
    /// refused a name the natives have or the file declares twice; a
    /// name another file of the request declared is
    /// `do_bind_function_error` at `E_COMPILE_ERROR`, which names the
    /// earlier declaration's file and line and ends the request before
    /// the frame is pushed, so nothing of a file that redeclares runs;
    /// `false` says so.
    pub(crate) fn declare_functions(&mut self, unit: &Arc<Unit>) -> bool {
        for (at, (decl, _)) in unit.functions.iter().enumerate() {
            let body = exec::Body::Function(u32::try_from(at).unwrap_or(u32::MAX));
            if self.preloaded_function(unit, body) {
                continue;
            }
            if let Some(earlier) = self.function_named(&decl.name.0) {
                let site = self
                    .user_function(earlier)
                    .and_then(user_function_decl)
                    .map(|decl| {
                        format!(
                            " (previously declared in {}:{})",
                            String::from_utf8_lossy(&decl.file.0),
                            decl.line
                        )
                    })
                    .unwrap_or_default();
                let message = format!(
                    "Cannot redeclare function {}(){site}",
                    String::from_utf8_lossy(&decl.name.0)
                );
                let file = decl.file.0.to_vec();
                self.end_in_compile_time(Level::CompileError, &message, &file, decl.line);
                return false;
            }
            self.user_functions.push(UserFunction {
                unit: Arc::clone(unit),
                body,
                member: None,
                flags: crate::attributes::Flags::of(&decl.attributes),
            });
        }
        true
    }

    pub(crate) fn class_named(&self, name: &[u8]) -> Option<ClassId> {
        class_in(&self.shared, &self.declared, &self.aliases, name)
    }

    /// The request's heap, which the units it compiles for itself intern
    /// into, beside its classes as the compiler asks after one
    /// ([`php_compile::RequestClasses`]), borrowed apart so both are
    /// handed to one compile.
    pub(crate) fn compiling<R>(
        &mut self,
        compile: impl FnOnce(&mut Heap, php_compile::RequestClasses<'_>) -> R,
    ) -> R {
        let (shared, declared, aliases) = (&self.shared, &self.declared, &self.aliases);
        let holds = |name: &[u8]| class_in(shared, declared, aliases, name).is_some();
        compile(&mut self.heap, php_compile::RequestClasses::new(&holds))
    }

    /// `zend_get_callable_name_ex` (`Z/Zend/zend_API.c:4075`): a string
    /// as it is written, a pair as `Class::method` with the class half
    /// as written or the object's class, a fake closure made of a method
    /// as `Scope::name` and any other closure by its function's name, an
    /// object of another class as `Class::__invoke`, and `Array` for a
    /// pair of any other shape. An object's class is named whole, so an
    /// anonymous class's name resolves back to the class:
    ///
    /// ```php
    /// new class {
    ///     function __construct() {
    ///         if (is_callable([$this, 'f'], false, $name)) { call_user_func($name); }
    ///     }
    ///     function f() { echo "nice\n"; }
    /// };
    /// ```
    /// ```expect
    /// nice
    /// ```
    pub(crate) fn callable_name(&self, value: &Value) -> String {
        let held = value.deref();
        if let Some(name) = held.as_str_bytes() {
            return String::from_utf8_lossy(name).into_owned();
        }
        if let Some(array) = held.as_array().copied() {
            let pair =
                (array.len() == 2).then(|| (array.get(&Key::Int(0)), array.get(&Key::Int(1))));
            if let Some((Some(first), Some(second))) = pair
                && let Some(method) = second.deref().as_str_bytes()
            {
                let method = String::from_utf8_lossy(method);
                let first = first.deref();
                if let Some(class) = first.as_str_bytes() {
                    return format!("{}::{method}", String::from_utf8_lossy(class));
                }
                if let Some(object) = first.as_object() {
                    return format!("{}::{method}", self.whole_class_name(object.class()));
                }
            }
            return String::from("Array");
        }
        if let Some(object) = held.as_object().copied() {
            if let Some(closure) = object.closure(self) {
                let name = self.declared_name(closure.function);
                return match closure.scope.filter(|_| closure.fake) {
                    Some(scope) => format!("{}::{name}", self.whole_class_name(scope)),
                    None => name,
                };
            }
            return format!("{}::__invoke", self.whole_class_name(object.class()));
        }
        String::new()
    }

    /// `ZSTR_VAL(ce->name)` at its whole length, as
    /// `zend_create_member_string` joins it: an anonymous class's name
    /// with its NUL and its site, which names the class again when the
    /// string is resolved.
    fn whole_class_name(&self, class: ClassId) -> String {
        String::from_utf8_lossy(&self.class_name_bytes(class)).into_owned()
    }

    /// The declaration of a linked class.
    #[must_use]
    pub fn class_decl(&self, class: ClassId) -> Option<&ClassDecl> {
        let at = usize::try_from(class.0).ok()?;
        match at.checked_sub(self.shared.classes.len()) {
            None => self.shared.classes.get(at),
            Some(declared) => self.declared.get(declared)?.decl(),
        }
    }

    /// `ce->ce_flags` of a linked class: its declaration's flags with
    /// what `zend_do_inheritance_ex` ORs in from its parent
    /// (`Z/Zend/zend_inheritance.c:2055`), `ZEND_ACC_NOT_SERIALIZABLE`
    /// and `ZEND_ACC_ALLOW_DYNAMIC_PROPERTIES`, the two of that mask a
    /// program can observe. A parent's flags already hold its own
    /// parent's, so the walk takes each class up the chain; a class
    /// that forbids dynamic properties keeps forbidding them, since
    /// `zend_std_write_property` reads `ZEND_ACC_NO_DYNAMIC_PROPERTIES`
    /// first. The one reader of the bits inheritance moves: what a
    /// dynamic property's creation and `serialize` ask of a class.
    /// The walk is cut at a depth no declaration can make.
    ///
    /// ```php
    /// class S extends stdClass {}
    /// $s = new S; $s->x = 1;
    /// #[AllowDynamicProperties] class P {}
    /// class C extends P {}
    /// class D extends C {}
    /// $d = new D; $d->y = 2;
    /// class R extends ReflectionClass {}
    /// class RR extends R {}
    /// try { serialize(new RR('stdClass')); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// class N {}
    /// class M extends N {}
    /// $m = new M; $m->z = 3;
    /// var_dump($s->x, $d->y);
    /// ```
    /// ```expect
    /// Serialization of 'RR' is not allowed
    ///
    /// Deprecated: Creation of dynamic property M::$z is deprecated in %s on line 13
    /// int(1)
    /// int(2)
    /// ```
    #[must_use]
    pub fn class_flags(&self, class: ClassId) -> Option<ClassFlags> {
        use php_unit::decl::DynamicProperties;
        let mut flags = self.class_decl(class)?.flags;
        let mut at = self.class_parent(class);
        for _ in 0..64 {
            let Some(parent) = at else { break };
            let Some(passed) = self.class_decl(parent).map(|decl| decl.flags) else {
                break;
            };
            flags.not_serializable |= passed.not_serializable;
            if passed.dynamic_properties == DynamicProperties::Allowed
                && flags.dynamic_properties == DynamicProperties::Deprecated
            {
                flags.dynamic_properties = DynamicProperties::Allowed;
            }
            at = self.class_parent(parent);
        }
        Some(flags)
    }

    /// `ce->create_object != NULL`: whether objects of a linked class are
    /// laid down by a `create_object` of its own or of the parent it
    /// inherits one from, a native payload (design §5.4), a throwable's
    /// `zend_default_exception_new` or `Closure`'s `zend_closure_new`,
    /// rather than by `zend_objects_new`.
    /// What `ReflectionClass::newInstanceWithoutConstructor` refuses an
    /// internal final class for.
    ///
    /// ```php
    /// foreach (['Closure', 'DOMException', 'ArithmeticError', 'stdClass'] as $c) {
    ///     try { (new ReflectionClass($c))->newInstanceWithoutConstructor(); echo "$c\n"; } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// Class Closure is an internal class marked as final that cannot be instantiated without invoking its constructor
    /// Class DOMException is an internal class marked as final that cannot be instantiated without invoking its constructor
    /// ArithmeticError
    /// stdClass
    /// ```
    #[must_use]
    pub fn class_has_create_object(&self, class: ClassId) -> bool {
        self.native_new_of(class).is_some()
            || self.records_throw_site(class)
            || crate::classes::core_create_object(self.handlers_of(class))
    }

    /// `ZEND_FLF_FUNC(opline)`: the native and the arity the engine's
    /// frameless table holds at `at`, a `FRAMELESS_ICALL_*`'s
    /// `extended_value`; none past the table, which a unit this engine
    /// compiled never carries.
    pub(crate) fn frameless_entry(&self, at: u32) -> Option<FramelessEntry> {
        self.shared.frameless.get(at as usize).copied()
    }

    /// `zend_flf_count`: how many rows the frameless table has.
    pub(crate) fn frameless_count(&self) -> usize {
        self.shared.frameless.len()
    }

    /// The declaration of a linked function.
    #[must_use]
    pub fn function_decl(&self, function: Function) -> Option<&FunctionDecl> {
        let at = usize::try_from(function.0).ok()?;
        if at < self.shared.functions.len() {
            return self.shared.functions.get(at);
        }
        if let Some(decl) = self.user_function(function).and_then(user_function_decl) {
            return Some(decl);
        }
        self.method_decl(function).map(|method| &method.function)
    }

    /// `get_declared_classes()`, in declaration order.
    #[must_use]
    pub fn declared_classes(&self) -> Vec<ClassId> {
        let count = self.shared.classes.len() + self.declared.len();
        (0..u32::try_from(count).unwrap_or(u32::MAX))
            .map(ClassId)
            .collect()
    }

    /// `get_defined_functions()` over `EG(function_table)`: `internal`,
    /// every native's key, and `user`, every user function's, each the
    /// lowered name the table is keyed by and in the order the table
    /// took them. A closure is no row, since php never adds one to the
    /// table; a method is none either, being its class's.
    pub fn defined_functions(&mut self) -> Result<PhpArray, Thrown> {
        let internal: Vec<Vec<u8>> = self
            .shared
            .functions
            .iter()
            .map(|decl| decl.name.0.to_ascii_lowercase())
            .collect();
        let user: Vec<Vec<u8>> = self
            .user_functions
            .iter()
            .filter_map(user_function_decl)
            .map(|decl| decl.name.0.to_ascii_lowercase())
            .filter(|name| !name.starts_with(b"{"))
            .collect();
        let mut array = PhpArray::new(self)?;
        for (list, names) in [(&b"internal"[..], internal), (b"user", user)] {
            let mut rows = PhpArray::new(self)?;
            for name in names {
                let name = Value::string(PhpString::new(self, &name)?);
                rows.append(self, name)?;
            }
            let key = Key::Str(PhpString::new(self, list)?);
            array.set(self, key, Value::array(rows))?;
        }
        Ok(array)
    }

    /// `zend_register_class_alias_ex`'s `zend_hash_add` into
    /// `EG(class_table)`: `class` under the lowered name `alias`, and
    /// `false` where the table has that name already, a class's or
    /// another alias's, and nothing is added. The name is the caller's
    /// to have lowered and held to php's reserved names.
    pub(crate) fn class_alias(&mut self, class: ClassId, alias: &[u8]) -> bool {
        if self.class_named(alias).is_some() {
            return false;
        }
        let place = self.buckets.take();
        self.aliases
            .push((place, alias.to_ascii_lowercase(), class));
        true
    }
}

/// The class `EG(class_table)` holds under `name`, any case: an engine's
/// class or a native class's alias, a class the request declared, then
/// an alias the request added; what [`Request::class_named`] answers.
/// Each is one lookup in its table's keys, as `zend_hash_find` over the
/// lowered name is: a class declared at `at` of the request's rows is
/// the `ClassId` past the engine's classes.
fn class_in(
    shared: &Shared,
    declared: &Keyed<Declared>,
    aliases: &Keyed<preload::Alias>,
    name: &[u8],
) -> Option<ClassId> {
    lowered_key(name, |key| {
        shared.class_keys.get(key).copied().or_else(|| {
            declared
                .find(key)
                .and_then(|(at, _)| u32::try_from(shared.classes.len().checked_add(at)?).ok())
                .map(ClassId)
                .or_else(|| aliases.find(key).map(|(_, (_, _, class))| *class))
        })
    })
}

/// The flags of the function at `at` of a unit's `dynamic` table, one
/// a `DECLARE_FUNCTION` binds or a closure is made of.
fn dynamic_flags(unit: &Unit, at: u32) -> crate::attributes::Flags {
    usize::try_from(at)
        .ok()
        .and_then(|at| unit.dynamic.get(at))
        .map(|(decl, _)| crate::attributes::Flags::of(&decl.attributes))
        .unwrap_or_default()
}

/// `zend_class_redeclaration_error`'s two wordings over the earlier
/// declaration: what it is and its own name, and its site when it was a
/// user class.
fn redeclaration(earlier: &ClassDecl) -> String {
    let what = php_unit::link::object_type(earlier.kind);
    let name = String::from_utf8_lossy(&earlier.name.0);
    if earlier.file.0.is_empty() {
        return format!("Cannot redeclare {what} {name}");
    }
    format!(
        "Cannot redeclare {what} {name} (previously declared in {}:{})",
        String::from_utf8_lossy(&earlier.file.0),
        earlier.line
    )
}
