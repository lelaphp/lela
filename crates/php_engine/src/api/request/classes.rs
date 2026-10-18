//! The classes and functions a request declares: `EG(class_table)`
//! past the engine's, a unit's classes declared at its load or at their
//! `DECLARE_CLASS` and `DECLARE_CLASS_DELAYED`, each linked with its
//! members, hooks and traits, anonymous classes, `class_alias()`, the
//! functions a unit declares and the closures it makes, and the lookups
//! by name and number that read them all. Which classes a load bound
//! early is [`Classes`]'s, whose field no file but this one reads.

use php_heap::{FrameRef, Intern};
use php_link::LinkedClass;
use php_unit::decl::{Bare, ClassDecl, ClassFlags, ClassKind, FunctionDecl, HookKind};
use php_unit::{Opcode, PublishedUnit, TopLevel, Unit};

use crate::api::call::{ClassNative, Function};
use crate::api::raising::{Level, Thrown, none_pending};
use crate::api::values::{ClassId, Key, Owner, PhpArray, PhpString, Value, adopted};
use crate::engine::Shared;
use crate::exec;

use super::{
    Bound, Declared, Deprecations, FrameShape, Keyed, Member, MemberAt, Named, Request,
    UserFunction, preload, user_function_decl,
};

/// A class's members before they are numbered ([`Request::members_of`]):
/// for each of its methods whether it is a native the class lays under
/// its own scope, an enum's own or a native trait's copied in, and the
/// rows of its methods and hooks, each method's beside its index.
pub(super) struct Members {
    served: Vec<Option<ClassNative>>,
    rows: Vec<MemberRow>,
}

/// One member's row before it is numbered: its index among the methods,
/// none for a hook, the function it is, and where its runtime cache is.
struct MemberRow {
    method: Option<u32>,
    user: UserFunction,
    cache: CacheOf,
}

/// Whose runtime cache a member's function reads: its op array's own
/// map pointer, or one of its own where the function is a copy of
/// another class's. php copies a trait's method into each class that
/// uses it (`zend_add_trait_method`) and a trait's hooked property's
/// hooks likewise (`zend_do_traits_property_binding`), and
/// `function_add_ref` gives each copy a fresh `run_time_cache` map
/// pointer (`Z/Zend/zend_compile.c:1256`), so each scope the trait's
/// code runs in has slots of its own. The copy shares the op array. A
/// class the request declares gives its copies caches of the request's
/// ([`exec::RuntimeCaches::own_cache`]); a class published into the
/// engine's arena, map pointers of the engine's, which every request
/// lays its own cache down at.
///
/// So a call through one opline of the trait's code primes a slot in
/// each class, and a class's `get_method` that would refuse an object
/// is not asked again where the slot answers, as in php:
///
/// ```php
/// class Lazy extends RecursiveIteratorIterator {
///     function __construct($init) { if ($init) parent::__construct(new RecursiveArrayIterator([])); }
///     function bar() { return "bar"; }
/// }
/// trait T { function call($it) { return $it->bar(); } }
/// class A { use T; }
/// class B { use T; }
/// foreach ([[new A, new Lazy(true)], [new B, new Lazy(true)], [new A, new Lazy(false)]] as [$by, $it]) {
///     try { echo $by->call($it), "\n"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// bar
/// bar
/// bar
/// ```
///
/// The same classes in a file opcache holds are linked once and
/// published, and their copies read the engine's map pointers:
///
/// ```php
/// $dir = sys_get_temp_dir() . '/traitcache' . bin2hex(random_bytes(8));
/// mkdir($dir);
/// file_put_contents("$dir/classes.php", '<?php class Lazy extends RecursiveIteratorIterator { function __construct($init) { if ($init) parent::__construct(new RecursiveArrayIterator([])); } function bar() { return "bar"; } } trait T { function call($it) { return $it->bar(); } } class A { use T; } class B { use T; }');
/// file_put_contents("$dir/main.php", '<?php require __DIR__ . "/classes.php"; foreach ([[new A, new Lazy(true)], [new B, new Lazy(true)], [new A, new Lazy(false)]] as [$by, $it]) { try { echo $by->call($it), "\n"; } catch (Error $e) { echo $e->getMessage(), "\n"; } }');
/// touch("$dir/classes.php", time() - 10);
/// echo shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 $dir/main.php");
/// array_map('unlink', glob("$dir/*")); rmdir($dir);
/// ```
/// ```expect process
/// bar
/// bar
/// bar
/// ```
#[derive(Clone, Copy, PartialEq, Eq)]
enum CacheOf {
    OpArray,
    Copy,
}

/// What numbering a class's members settles: the function each of its
/// methods is, by its index, and the numbers its members took.
pub(super) type Numbered = (Box<[Option<Function>]>, std::ops::Range<u32>);

impl Members {
    /// The function each method is, the members numbered from `numbers`
    /// in order and a class native left for its caller.
    fn functions(&self, numbers: std::ops::Range<u32>) -> Vec<Option<Function>> {
        let mut functions = vec![None; self.served.len()];
        for (number, row) in numbers.zip(&self.rows) {
            if let Some(slot) =
                (row.method).and_then(|method| functions.get_mut(usize::try_from(method).ok()?))
            {
                *slot = Some(Function(number));
            }
        }
        functions
    }
}

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
    bound_at_load: Vec<(PublishedUnit, u32)>,
}

/// How opcache served a unit, `zend_accel_load_script`'s: from what it
/// held, or compiled by this request's miss, whose compile binds the
/// file's functions into the request's own table before the load
/// copies them (`Z/ext/opcache/ZendAccelerator.c:1855`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccelServed {
    Hit,
    Compiled,
}

/// What a unit's load has declared of its classes, as
/// [`Request::class_load`] lays it down: how opcache served the unit,
/// none where php's own compiler declares it, the keys its delayed
/// early bindings stand under, the classes published with it, the
/// delayed ones left for after the rest, and how many of the unit's
/// classes were met so far.
pub(crate) struct ClassLoad {
    served: Option<AccelServed>,
    delayed: Vec<Vec<u8>>,
    early: Option<crate::unit_cache::EarlyBound>,
    tried: Vec<usize>,
    next: usize,
}

impl ClassLoad {
    /// Whether opcache serves the unit, so its load tells the observers
    /// in the order it copies the tables.
    pub(crate) fn accelerated(&self) -> bool {
        self.served.is_some()
    }
}

impl Request {
    /// `EG(class_table)`'s keys in their order, each row the class and,
    /// for an alias's row, the lowered name it is keyed by: the engine's
    /// classes, each native alias after its class, then the request's
    /// rows in their buckets. A class is a user class where it has no
    /// [`Request::class_extension`]; `get_declared_classes()` lists these
    /// rows, and phpdbg's `clean` counts them.
    #[must_use]
    pub fn class_keys(&self) -> Vec<(ClassId, Option<Vec<u8>>)> {
        let mut rows: Vec<(ClassId, Option<Vec<u8>>)> = (0u32..)
            .zip(&self.shared.classes)
            .flat_map(|(at, decl)| {
                let aliases = decl.aliases.iter();
                std::iter::once(None)
                    .chain(aliases.map(|alias| Some(alias.0.to_ascii_lowercase())))
                    .map(move |alias| (ClassId(at), alias))
            })
            .collect();
        let mut own: Vec<(preload::Bucket, ClassId, Option<Vec<u8>>)> = self
            .declared
            .iter()
            .map(|named| (named.place, named.class, None))
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

    /// The load of a unit's classes, `do_bind_class` for every class the
    /// unit declares that php bound while the file compiled, before its
    /// main op array runs: a
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
    ///
    /// Each class linked is told to the observers as it is linked
    /// ([`crate::observer::class_linked_at_load`]): every class bound
    /// early, each delayed one the load binds, and an anonymous class
    /// linked here only where opcache serves the unit, whose load copies
    /// the class `link_unbound` linked with the rest, where php's own
    /// compiler tells no observer of it. This lays down the load's state
    /// over `unit`'s classes, for [`Request::declare_classes_through`]
    /// and [`Request::declare_delayed`].
    pub(crate) fn class_load(&mut self, unit: &PublishedUnit) -> ClassLoad {
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
        let loaded = (self.accel_loaded.take())
            .filter(|(loaded, _)| PublishedUnit::ptr_eq(loaded, unit))
            .map(|(_, served)| served);
        let served = loaded.or_else(|| self.preloading.is_some().then_some(AccelServed::Hit));
        let early = (self.early_bound.take())
            .filter(|(loaded, _)| PublishedUnit::ptr_eq(loaded, unit))
            .map(|(_, early)| early);
        ClassLoad {
            served,
            delayed,
            early,
            tried: Vec::new(),
            next: 0,
        }
    }

    /// `unit`'s classes declared in their order up to and including the
    /// one at `through`, or every one left where `through` is none,
    /// each linked told as it is: false where the request ended.
    pub(crate) fn declare_classes_through(
        &mut self,
        unit: &PublishedUnit,
        load: &mut ClassLoad,
        through: Option<usize>,
    ) -> bool {
        let last = through.map_or(unit.classes.len(), |at| at.saturating_add(1));
        while load.next < last.min(unit.classes.len()) {
            let at = load.next;
            load.next += 1;
            let Some(decl) = unit.classes.get(at) else {
                break;
            };
            match self.declare_class_at(unit, load, at, decl) {
                Some(true) => crate::observer::class_linked_at_load(self, unit, at),
                Some(false) => {}
                None => return false,
            }
        }
        true
    }

    /// The class at `at` of `unit` declared as the load meets it: none
    /// where the request ended, and whether it was linked and is told
    /// otherwise. A delayed one opcache serves is left for
    /// [`Request::declare_delayed`].
    fn declare_class_at(
        &mut self,
        unit: &PublishedUnit,
        load: &mut ClassLoad,
        at: usize,
        decl: &ClassDecl,
    ) -> Option<bool> {
        // A saved script of the preload, whose classes the preload
        // holds, declares none of them again (`preload_remove_declares`).
        if self.preloaded_class(unit, at) {
            return Some(false);
        }
        let key = unit
            .runtime_keys
            .iter()
            .find(|(_, class)| usize::try_from(*class).ok() == Some(at));
        if let Some((key, _)) = key {
            self.buckets
                .reserve(unit, u32::try_from(at).unwrap_or(u32::MAX));
            if decl.flags.is_anonymous {
                return Some(self.link_unbound(unit, at, decl)? && load.accelerated());
            }
            if load.delayed.iter().any(|delayed| **delayed == *key.0) {
                if load.accelerated() {
                    load.tried.push(at);
                    return Some(false);
                }
                return self.bind_at_load(unit, at, decl);
            }
            return Some(false);
        }
        if let Some(existing) = self.class_named((&decl.name).into()) {
            let message = self.redeclaration(existing);
            let file = decl.file.0.to_vec();
            self.end_in_compile_time(Level::CompileError, &message, &file, decl.line);
            return None;
        }
        let published = load.early.as_ref().and_then(|early| *early.get(at)?);
        let at = u32::try_from(at).ok()?;
        match published {
            Some(class) => self.declare_published(unit, at, class, decl),
            None => self.bind_declared(unit, at, None, None),
        }
        Some(true)
    }

    /// The delayed early bindings opcache's load makes once the unit's
    /// classes are declared, `zend_accel_do_delayed_early_binding`, each
    /// bound told: false where the request ended.
    pub(crate) fn declare_delayed(&mut self, unit: &PublishedUnit, load: &mut ClassLoad) -> bool {
        for at in std::mem::take(&mut load.tried) {
            let Some(decl) = unit.classes.get(at) else {
                continue;
            };
            match self.bind_at_load(unit, at, decl) {
                Some(true) => crate::observer::class_linked_at_load(self, unit, at),
                Some(false) => {}
                None => return false,
            }
        }
        true
    }

    /// One entry of `zend_accel_do_delayed_early_binding`: the class at
    /// `at` of `unit` bound now where its name is free and its parent,
    /// if it names one, is already declared, looked up without
    /// autoloading, then linked as the early bind links
    /// ([`Request::link_declared`] under `early`), and recorded so its
    /// opline passes over it. The class stands under its runtime
    /// definition key alone, as at the compile-time early bind
    /// ([`php_link::early_bind`]), so a check its parent's member makes
    /// naming it does not find it and leaves the class to the opline,
    /// which raises whatever the link refuses after the code above it;
    /// a link refused with nothing left unresolved ends the request
    /// before the file runs, as php's compile-time early bind would. None
    /// when the request ended, and whether the class was bound otherwise.
    ///
    /// ```php
    /// echo "ran\n";
    /// class A { function g(B $b) {} function f(int $a) {} }
    /// class B extends A { function g(A $b) {} function f(string $a) {} }
    /// ```
    /// ```expect exit=255
    /// ran
    ///
    /// Fatal error: Declaration of B::f(string $a) must be compatible with A::f(int $a) in %s on line 4
    /// ```
    ///
    /// ```php
    /// var_dump(class_exists('B', false));
    /// class A { public function m(B|string $a): A|string { return 'A'; } }
    /// class B extends A { public function m(A|string $a): B|string { return 'B'; } }
    /// var_dump(class_exists('B', false));
    /// ```
    /// ```expect exit=0
    /// bool(false)
    /// bool(true)
    /// ```
    ///
    /// An anonymous class whose `set` hook's parameter type the file
    /// could not hold to its property's is linked at its
    /// `DECLARE_ANON_CLASS` ([`Request::link_unbound`]), the refusal raised
    /// there:
    ///
    /// ```php
    /// echo "before\n";
    /// $o = new class { public X $p { set(Y $v) {} } };
    /// class X {}
    /// class Y {}
    /// ```
    /// ```expect exit=255
    /// before
    ///
    /// Fatal error: Type of parameter $v of hook class@anonymous::$p::set must be compatible with property type in %s on line 3
    /// ```
    fn bind_at_load(&mut self, unit: &PublishedUnit, at: usize, decl: &ClassDecl) -> Option<bool> {
        if self.class_named((&decl.name).into()).is_some() {
            return Some(false);
        }
        if let Some(parent) = &decl.parent
            && self.class_named(parent.into()).is_none()
        {
            return Some(false);
        }
        match self.link_declared(unit, at, decl, true) {
            Ok(true) => {
                let at = u32::try_from(at).unwrap_or(u32::MAX);
                self.classes
                    .bound_at_load
                    .push((PublishedUnit::clone(unit), at));
                Some(true)
            }
            Ok(false) => Some(false),
            Err(_) => None,
        }
    }

    /// `zend_compile_class_decl`'s `link_unbound` for the anonymous class
    /// at `at` of `unit`: one with no parent, no interface, no trait and
    /// no hook variance check left for the link
    /// ([`php_unit::decl::ClassFlags::hooked_variance`]) php links while
    /// the file compiles and holds in `CG(class_table)`
    /// from then on, so it is linked and bound as the unit loads, and
    /// its `DECLARE_ANON_CLASS` finds it by name
    /// ([`Request::declare_anonymous`]); any other is linked by the
    /// opline. A name already bound is the same unit loaded again, whose
    /// class stands. None when the link ended the request, and whether
    /// this link bound the class otherwise.
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
    fn link_unbound(&mut self, unit: &PublishedUnit, at: usize, decl: &ClassDecl) -> Option<bool> {
        let unbound = decl.parent.is_none()
            && decl.interfaces.is_empty()
            && decl.traits.is_empty()
            && !decl.flags.hooked_variance;
        if !unbound || self.class_named((&decl.name).into()).is_some() {
            return Some(false);
        }
        self.link_declared(unit, at, decl, true).ok()
    }

    /// `ZEND_DECLARE_CLASS_DELAYED` (`Z/Zend/zend_vm_def.h:7968`):
    /// nothing where the unit's load bound the class under the opline's
    /// runtime definition key ([`Request::bind_at_load`]), and
    /// [`Request::bind_class`] where it did not, the key still standing.
    pub(crate) fn bind_delayed(&mut self, unit: &PublishedUnit, key: &[u8]) -> Result<(), Thrown> {
        let at = unit
            .runtime_keys
            .iter()
            .find(|(filed, _)| *filed.0 == *key)
            .map(|(_, at)| *at);
        if let Some(bound) = self
            .classes
            .bound_at_load
            .iter()
            .position(|(held, class)| PublishedUnit::ptr_eq(held, unit) && Some(*class) == at)
        {
            self.classes.bound_at_load.swap_remove(bound);
            return Ok(());
        }
        self.bind_class_in_slot(unit, key)
    }

    /// `zend_bind_class_in_slot` (`Z/Zend/zend_compile.c:1304-1345`), what a
    /// `DECLARE_CLASS` and a `DECLARE_CLASS_DELAYED` the load did not
    /// bind make: the class bound ([`Request::bind_class`]), and the
    /// observers told it is linked under its lowered name, which
    /// `DECLARE_ANON_CLASS`'s own bind does not tell.
    pub(crate) fn bind_class_in_slot(
        &mut self,
        unit: &PublishedUnit,
        key: &[u8],
    ) -> Result<(), Thrown> {
        self.bind_class(unit, key)?;
        if !crate::observer::classes_linked_observed(self) {
            return Ok(());
        }
        let at = unit
            .runtime_keys
            .iter()
            .find(|(filed, _)| *filed.0 == *key)
            .and_then(|(_, at)| usize::try_from(*at).ok());
        // The opline of a class the preload holds is a `NOP`, which tells
        // nothing.
        let name = at
            .filter(|&at| !self.preloaded_class(unit, at))
            .and_then(|at| unit.classes.get(at))
            .map(|decl| decl.name.0.to_ascii_lowercase());
        if let Some(name) = name
            && let Some(class) = self.class_named(Bare::resolved(&name))
        {
            crate::observer::class_linked(self, class, &name);
        }
        Ok(())
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
    /// methods into the function table beside it and `record`, the
    /// record its link made, kept as it bound ([`Request::bind_members`]),
    /// which is the whole of what binding a class is here. It stands in
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
        unit: &PublishedUnit,
        at: u32,
        composed: Option<php_link::Composed>,
        record: Option<LinkedClass<ClassId>>,
    ) {
        let Some(class) = ClassId::local(self.own_classes.len()) else {
            return;
        };
        let place = self.buckets.claim(unit, at);
        let row = Declared {
            unit: PublishedUnit::clone(unit),
            at,
            composed: composed.map(Box::new),
            bound: Bound::default(),
        };
        if let Some(decl) = row.decl() {
            self.declared.push(&decl.name.0, Named { place, class });
        }
        self.own_classes.push(row);
        self.bind_members(class, record);
    }

    /// `zend_accel_class_hash_copy` for one class a held unit published
    /// with itself (design §6.3, [`Request::publish_unit`]): the class's
    /// number filed under its lowered name in the bucket its load takes,
    /// and nothing else. Its record, its methods and its placed
    /// properties are the published row's; what the request changes of
    /// it, its statics and the constants and defaults it evaluates, the
    /// request keeps under the number, so the next request reads them
    /// fresh. Served by the built-in server, whose requests share the
    /// unit cache, the second request serves the unit the first one held:
    ///
    /// ```php
    /// $dir = sys_get_temp_dir() . '/early' . bin2hex(random_bytes(8));
    /// mkdir($dir);
    /// file_put_contents("$dir/index.php", '<?php define("V", $_GET["v"]);
    /// class P { const K = V . "k"; public static $s = V; public $d = [V]; function f() { return __CLASS__; } }
    /// class C extends P { public $e = self::K; }
    /// $c = new C;
    /// echo C::K, " ", C::$s, " ", $c->d[0], " ", $c->e, " ", $c->f(), " ";
    /// C::$s .= "!"; P::$s .= "?";
    /// echo C::$s, " ", implode(",", array_slice(get_declared_classes(), -2)), "\n";');
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable=1', '-d', 'opcache.jit=off', '-S', '127.0.0.1:0', '-t', $dir], [1 => ['file', '/dev/null', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// $ready = [$pipes[2]]; $none = null;
    /// $line = stream_select($ready, $none, $none, 30) ? fgets($pipes[2]) : '';
    /// $port = preg_match('/:(\d+)\) started$/', rtrim($line), $m) ? $m[1] : 0;
    /// foreach (['a', 'b'] as $v) { echo file_get_contents("http://127.0.0.1:$port/?v=$v"); }
    /// proc_terminate($p); proc_close($p);
    /// unlink("$dir/index.php"); rmdir($dir);
    /// ```
    /// ```expect process
    /// ak a a ak P a!? P,C
    /// bk b b bk P b!? P,C
    /// ```
    pub(super) fn declare_published(
        &mut self,
        unit: &PublishedUnit,
        at: u32,
        class: ClassId,
        decl: &ClassDecl,
    ) {
        let place = self.buckets.claim(unit, at);
        self.declared.push(&decl.name.0, Named { place, class });
    }

    /// The members of `class`, bound, into the function table beside it:
    /// an enum's own `cases`, `from` and `tryFrom`, each method, and
    /// each hook of the class's own properties; and the class's record,
    /// the one its link made or else made here, which every lookup by
    /// name after reads ([`Request::method_named`]), an enum's three as
    /// the functions that serve them:
    ///
    /// ```php
    /// trait T { function fromTrait() { return "t"; } }
    /// class P { function Inherited() { return "p"; } private function hidden() {} }
    /// class C extends P { use T; function OWN() { return "c"; } }
    /// enum E: string { case A = "a"; function label() { return "l"; } }
    /// $c = new C;
    /// echo $c->own(), $c->INHERITED(), $c->FromTrait(), E::FROM("a")->Label(), "\n";
    /// var_dump(method_exists($c, "HIDDEN"), method_exists("C", "nope"), is_callable([$c, "fromtrait"]), E::tryfrom("z"));
    /// var_dump(get_class_methods("C"));
    /// ```
    /// ```expect
    /// cptl
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// NULL
    /// array(3) {
    ///   [0]=>
    ///   string(3) "OWN"
    ///   [1]=>
    ///   string(9) "fromTrait"
    ///   [2]=>
    ///   string(9) "Inherited"
    /// }
    /// ```
    pub(super) fn bind_members(&mut self, class: ClassId, record: Option<LinkedClass<ClassId>>) {
        let parent = self
            .class_decl(class)
            .and_then(|decl| self.class_named(decl.parent.as_ref()?.into()));
        let Some(members) = self
            .declared_row(class)
            .and_then(|row| self.members_of(row, class))
        else {
            return;
        };
        let Some((functions, members)) = self.place_own(class, members) else {
            return;
        };
        let record = record.or_else(|| self.record_at_bind(class));
        let placed = self
            .class_decl(class)
            .map_or_else(super::Placed::default, |decl| {
                self.placed_over(class, decl, record.as_ref(), parent)
            });
        if let Some(declared) = self.declared_mut(class) {
            declared.bound = Bound {
                record,
                functions,
                members,
                parent,
                placed,
            };
        }
    }

    /// The placed properties of `class`, declared by `decl`, over its
    /// parent's, read off `record` ([`super::placed_of`]): built once as
    /// the class binds, so a publisher hands the number on with them
    /// built.
    pub(super) fn placed_over(
        &self,
        class: ClassId,
        decl: &ClassDecl,
        record: Option<&LinkedClass<ClassId>>,
        parent: Option<ClassId>,
    ) -> super::Placed {
        let above = parent.map_or(&[][..], |parent| self.placed_properties(parent));
        super::placed_of(class, decl, record, above, |info| self.property_decl(info))
    }

    /// The members the class `row` declares binding settles under the
    /// number `class`, before they are numbered: each method and each
    /// hook of its own properties a user function's row, and which of its
    /// methods are natives it lays under its own scope, an enum's own
    /// `cases`, `from` and `tryFrom` and a native trait's methods.
    pub(super) fn members_of(&self, row: &Declared, class: ClassId) -> Option<Members> {
        let decl = row.decl()?;
        let enum_backing = (decl.kind == ClassKind::Enum).then_some(decl.backing);
        let served: Vec<Option<ClassNative>> = (0u32..)
            .zip(&decl.methods)
            .map(|(method, decl)| {
                // `zend_enum_register_funcs` at the link: the enum's own
                // `cases`, `from` and `tryFrom`, which the compiler
                // declared after its methods and which no body of the
                // unit serves; else `zend_add_trait_method`'s copy of a
                // native trait's method, scoped to this class.
                let key = decl.function.name.0.to_ascii_lowercase();
                let own =
                    enum_backing.and_then(|backing| crate::enums::EnumMethod::of(&key, backing));
                own.map(ClassNative::Enum).or_else(|| {
                    self.native_trait_method(row, method)
                        .map(|native| ClassNative::Trait { native, at: method })
                })
            })
            .collect();
        // The rows the class's members take, in the order php's tables
        // list them, each method's beside its place among the methods.
        let mut rows: Vec<MemberRow> = Vec::new();
        for ((method, served), decl) in (0u32..).zip(&served).zip(&decl.methods) {
            if served.is_some() {
                continue;
            }
            let Some((unit, body, cache)) = self.method_body(row, method) else {
                continue;
            };
            let member = Some(Member {
                class,
                at: MemberAt::Method(method),
            });
            let flags = crate::attributes::Flags::of(&decl.function.attributes);
            let shape = FrameShape::of(&decl.function, exec::body_of(&unit, body));
            let record = UserFunction::record(&unit, body, &decl.function.name.0);
            rows.push(MemberRow {
                method: Some(method),
                user: UserFunction {
                    unit,
                    body,
                    member,
                    flags,
                    shape,
                    record,
                },
                cache,
            });
        }
        // Each hook of the class's own properties, `prop_info->hooks`,
        // a function of its own that php's function table never lists.
        for (property, declared) in (0u32..).zip(&decl.properties) {
            for kind in HookKind::ALL {
                let Some(hook) = declared.hooks.as_ref().and_then(|hooks| hooks.of(kind)) else {
                    continue;
                };
                let Some((unit, body, cache)) = self.hook_body(row, property, kind) else {
                    continue;
                };
                let member = Some(Member {
                    class,
                    at: MemberAt::Hook { property, kind },
                });
                let flags = crate::attributes::Flags::of(&hook.function.attributes);
                let shape = FrameShape::of(&hook.function, exec::body_of(&unit, body));
                let record = UserFunction::record(&unit, body, &hook.function.name.0);
                rows.push(MemberRow {
                    method: None,
                    user: UserFunction {
                        unit,
                        body,
                        member,
                        flags,
                        shape,
                        record,
                    },
                    cache,
                });
            }
        }
        Some(Members { served, rows })
    }

    /// A class of the request's own's members numbered among the
    /// request's rows: the function each method is and the numbers its
    /// members took.
    fn place_own(&mut self, class: ClassId, members: Members) -> Option<Numbered> {
        let first = Function::local(self.user_functions.len())?.0;
        let count = u32::try_from(members.rows.len()).ok()?;
        let numbers = first..first.checked_add(count)?;
        let mut functions = members.functions(numbers.clone());
        for MemberRow {
            mut user, cache, ..
        } in members.rows
        {
            // A copy's runtime cache is the request's, as the class is,
            // held for the rest of the request.
            if cache == CacheOf::Copy {
                user.shape.cache = self.map.own_cache(None);
            }
            self.user_functions.push(user);
        }
        for (slot, served) in functions.iter_mut().zip(members.served) {
            if let Some(served) = served {
                *slot = Function::local_class_native(self.class_natives.len());
                self.class_natives.push((class, served));
            }
        }
        Some((functions.into_boxed_slice(), numbers))
    }

    /// A published class's members published into the engine's arena
    /// (design §5.14): the function each method is and the numbers its
    /// members took. None where the arena has no room for them.
    pub(super) fn place_published(&self, class: ClassId, members: Members) -> Option<Numbered> {
        let count = u32::try_from(members.rows.len()).ok()?;
        let numbers = self.arena.reserve_functions(count)?;
        let mut functions = members.functions(numbers.clone());
        for (
            number,
            MemberRow {
                mut user, cache, ..
            },
        ) in numbers.clone().zip(members.rows)
        {
            // A copy published with its class takes a map pointer of the
            // engine's, `ZEND_MAP_PTR_NEW`, as a published op array does,
            // so every request lays its own cache down at that offset.
            if cache == CacheOf::Copy {
                user.shape.cache = crate::engine::new_map_ptr(&self.shared.map_ptr_last);
            }
            self.arena
                .publish_function(Function(number), user)
                .then_some(())?;
        }
        for (slot, served) in functions.iter_mut().zip(members.served) {
            if let Some(served) = served {
                let function = self.arena.reserve_class_native()?;
                self.arena
                    .publish_class_native(function, (class, served))
                    .then_some(())?;
                *slot = Some(function);
            }
        }
        Some((functions.into_boxed_slice(), numbers))
    }

    /// The row of a class the request declared, or the engine published
    /// into its arena, the preload's among them; none for an engine's
    /// class.
    pub(super) fn declared_row(&self, class: ClassId) -> Option<&Declared> {
        match class.local_at() {
            Some(at) => self.own_classes.get(at),
            None => self.arena.class(class),
        }
    }

    /// The row of a class the request declared, to bind; none for an
    /// engine's class and for a published one.
    fn declared_mut(&mut self, class: ClassId) -> Option<&mut Declared> {
        self.own_classes.get_mut(class.local_at()?)
    }

    /// Where the op array of a hook of the property at `property` of a
    /// class the request declared is: its own unit's body, or for a
    /// property a trait gave the class the body in the trait's unit,
    /// which php shares between the copies as it shares a method's
    /// (`zend_do_traits_property_binding`).
    fn hook_body<'r>(
        &'r self,
        declared: &'r Declared,
        property: u32,
        kind: HookKind,
    ) -> Option<(PublishedUnit, exec::Body, CacheOf)> {
        let decl = declared.decl()?;
        let name = &decl.properties.get(usize::try_from(property).ok()?)?.name;
        let own = declared
            .unit
            .classes
            .get(usize::try_from(declared.at).ok()?)?;
        let (from, at, cache) = match own.properties.iter().position(|p| p.name == *name) {
            Some(at) => (declared, at, CacheOf::OpArray),
            None => decl.traits.iter().find_map(|trait_name| {
                let holder = self.class_named(trait_name.into())?;
                let from = self.declared_row(holder)?;
                let at = from
                    .decl()?
                    .properties
                    .iter()
                    .position(|p| p.name == *name && p.hooks.is_some())?;
                Some((from, at, CacheOf::Copy))
            })?,
        };
        let body = exec::Body::hook(&from.unit, from.at, u32::try_from(at).ok()?, kind)?;
        Some((PublishedUnit::clone(&from.unit), body, cache))
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
    /// The native that serves method number `method` of a composed
    /// class, where a native trait gave the class that method; none
    /// for a method a program's code serves.
    fn native_trait_method(&self, declared: &Declared, method: u32) -> Option<Function> {
        let composed = declared.composed.as_ref()?;
        let body = composed.bodies.get(usize::try_from(method).ok()?)?;
        if composed.decl.name.0.eq_ignore_ascii_case(&body.class.0) {
            return None;
        }
        let holder = self.class_named((&body.class).into())?;
        self.native_class_at(holder)?;
        self.method_function(holder, body.method)
    }

    fn method_body<'r>(
        &'r self,
        declared: &'r Declared,
        method: u32,
    ) -> Option<(PublishedUnit, exec::Body, CacheOf)> {
        let Some(composed) = &declared.composed else {
            let body = exec::Body::method(&declared.unit, declared.at, method)?;
            return Some((PublishedUnit::clone(&declared.unit), body, CacheOf::OpArray));
        };
        let body = composed.bodies.get(usize::try_from(method).ok()?)?;
        let own = composed.decl.name.0.eq_ignore_ascii_case(&body.class.0);
        let (from, cache) = if own {
            (declared, CacheOf::OpArray)
        } else {
            let holder = self.class_named((&body.class).into())?;
            (self.declared_row(holder)?, CacheOf::Copy)
        };
        let body = exec::Body::method(&from.unit, from.at, body.method)?;
        Some((PublishedUnit::clone(&from.unit), body, cache))
    }

    /// `do_bind_class` at a `ZEND_DECLARE_CLASS`: the declaration the
    /// unit files under the opline's runtime definition key
    /// ([`Unit::runtime_keys`]), refused where its name is already
    /// declared as `zend_class_redeclaration_error` refuses it, at
    /// `E_COMPILE_ERROR` though the program is running, which is also
    /// what the same opline running a second time meets; then
    /// `zend_do_link_class`, the one link the early bind makes too
    /// (`php_link::link`), with the parent fetched first, then
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
    /// ([`php_link::Composed`]), which stands in the table in its
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
    /// `zend_inheritance.c` makes, [`php_link`], which the early
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
    pub(crate) fn bind_class(&mut self, unit: &PublishedUnit, key: &[u8]) -> Result<(), Thrown> {
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
            .class_named((&decl.name).into())
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
        unit: &PublishedUnit,
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
        if let Some(class) = self.class_named(Bare::resolved(&name)) {
            return Ok(class);
        }
        self.bind_class(unit, key)?;
        self.class_named(Bare::resolved(&name)).ok_or_else(|| {
            crate::api::raising::fatal(
                self,
                "InternalError: an anonymous class its declaration did not bind",
            )
        })
    }

    /// `zend_do_link_class` over the class at `at` of `unit`, whose
    /// parent, traits and interfaces the table already holds: the link
    /// both the early bind and a `DECLARE_CLASS` make
    /// (`php_link::link`), its diagnostics raised at their sites,
    /// a refusal the end of the request at its level, and the linked
    /// class bound. A link that needs a class the table does not hold
    /// yet is php's `INHERITANCE_UNRESOLVED`: under `early`, the answer
    /// `zend_can_early_bind` gives before anything is raised or bound,
    /// so the class is left for its opline and the answer is false;
    /// otherwise the refusal it names.
    fn link_declared(
        &mut self,
        unit: &PublishedUnit,
        at: usize,
        decl: &ClassDecl,
        early: bool,
    ) -> Result<bool, Thrown> {
        let Ok(at) = u32::try_from(at) else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a unit declares more classes than can be numbered",
            ));
        };
        // `zend_inheritance_cache_get` before anything is checked, over
        // what was fetched (design §6.2).
        let fetched = self.fetched(unit, decl);
        if let Some(fetched) = &fetched
            && let Some(entry) = self.cached_link(unit, at, fetched)?
        {
            self.declare_cached(unit, at, decl, &entry);
            return Ok(true);
        }
        let (linking, read) = self.link_reading(decl, early);
        if early && !linking.obligations.is_empty() {
            return Ok(false);
        }
        if let Some(fetched) = fetched
            && let Some(entry) = self.publish_link(unit, at, fetched, &linking, &read, early)
        {
            self.declare_cached(unit, at, decl, &entry);
            return Ok(true);
        }
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
        none_pending(self)?;
        Ok(())
    }

    /// A class as the link reads the table: its declaration, and the
    /// bodies of its methods where it was composed with traits.
    pub(super) fn held_class(&self, name: Bare<'_>) -> Option<php_link::Held<'_, ClassId>> {
        self.held_as(self.class_named(name)?)
    }

    /// [`Request::held_class`] of the class `class` names.
    fn held_as(&self, class: ClassId) -> Option<php_link::Held<'_, ClassId>> {
        let decl = self.class_decl(class)?;
        let bodies = self
            .declared_row(class)
            .and_then(|declared| declared.composed.as_ref())
            .map(|composed| composed.bodies.as_slice());
        Some(php_link::Held {
            decl,
            bodies,
            linked: true,
            record: self.linked_class(class).map(|record| (class, record)),
        })
    }

    /// The record of `class`, bound by no link of the engine's, which the
    /// compiler's early bind checked: made over the table as it stands,
    /// whose every class the class names is linked. The traits' abstract
    /// methods a composed class took stand after its parent's.
    fn record_at_bind(&self, class: ClassId) -> Option<LinkedClass<ClassId>> {
        let row = self.declared_row(class)?;
        self.record_over(row, &|name| self.class_named(Bare::resolved(name)))
    }

    /// The record of the class `row` declares over the classes `named`
    /// finds by name, which a publication points at the published ones
    /// ([`Request::record_at_bind`]).
    pub(super) fn record_over(
        &self,
        row: &Declared,
        named: &dyn Fn(&[u8]) -> Option<ClassId>,
    ) -> Option<LinkedClass<ClassId>> {
        let decl = row.decl()?;
        let abstracts = row
            .composed
            .as_ref()
            .map_or(0, |composed| composed.abstracts);
        let lookup = |name: &[u8]| self.held_as(named(name)?);
        let holder = |holder: ClassId| self.class_decl(holder);
        php_link::linked_class(
            decl,
            usize::try_from(abstracts).unwrap_or(usize::MAX),
            &lookup,
            &holder,
        )
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
        let found = crate::api::call::autoload(self, &name.0)?;
        found.ok_or_else(|| {
            crate::api::raising::throw_error(
                self,
                "Error",
                &format!("{what} \"{}\" not found", String::from_utf8_lossy(&name.0)),
            )
        })
    }

    /// `zend_hash_find(EG(function_table), lcname)`: the native of that
    /// name, or the user function the request's table holds under it,
    /// one hash lookup in each, whatever the name's case.
    pub(crate) fn function_named(&self, name: Bare<'_>) -> Option<Function> {
        if let Some(i) = self.shared.function_at(name.bytes()) {
            return u32::try_from(i).ok().map(Function);
        }
        self.functions.find(name.bytes()).copied()
    }

    /// `EG(function_table)` in its order: every native function the
    /// table holds, so none `disable_functions` took out, then
    /// each user function the request can find, in the order the table
    /// took them, as `get_defined_functions()` lists them; a method is
    /// none, being its class's, and a closure is none, since php never
    /// adds one to the table. A function is a user function where it
    /// has an op array ([`Request::op_array`]).
    #[must_use]
    pub fn functions(&self) -> Vec<Function> {
        let natives = u32::try_from(self.shared.functions.len()).unwrap_or(u32::MAX);
        (0..natives)
            .filter(|&at| self.shared.in_function_table(at as usize))
            .map(Function)
            .chain(self.functions.iter().copied())
            .collect()
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
        unit: &PublishedUnit,
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
        if let Some(earlier) = self.function_named(Bare::resolved(lowered)) {
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
        let declared = Function::local(self.user_functions.len());
        let (flags, shape) = dynamic_traits(unit, at);
        self.user_functions.push(UserFunction {
            unit: PublishedUnit::clone(unit),
            body: exec::Body::Dynamic(at),
            member: None,
            flags,
            shape,
            record: UserFunction::record(unit, exec::Body::Dynamic(at), &decl.name.0),
        });
        // `do_bind_function`'s `zend_observer_function_declared_notify`
        // (`Z/Zend/zend_compile.c:1300`).
        if let Some(declared) = declared {
            self.functions.push(&decl.name.0, declared);
            crate::observer::function_declared(self, declared, lowered);
        }
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
    pub(crate) fn closure_function(&mut self, unit: &PublishedUnit, at: u32) -> Option<Function> {
        unit.dynamic.get(usize::try_from(at).ok()?)?;
        let body = exec::Body::Dynamic(at);
        if let Some(position) = self
            .user_functions
            .iter()
            .position(|user| user.body == body && PublishedUnit::ptr_eq(&user.unit, unit))
        {
            return Function::local(position);
        }
        let function = Function::local(self.user_functions.len())?;
        let (flags, shape) = dynamic_traits(unit, at);
        let name = unit
            .dynamic
            .get(usize::try_from(at).ok()?)
            .map_or(&[][..], |(decl, _)| &decl.name.0);
        self.user_functions.push(UserFunction {
            unit: PublishedUnit::clone(unit),
            body,
            member: None,
            flags,
            shape,
            record: UserFunction::record(unit, body, name),
        });
        Some(function)
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
        self.scope_of(self.frame_at(frame)?)
    }

    /// [`Request::frame_scope`] of a frame, on whichever stack it stands.
    pub(crate) fn scope_of(&self, state: exec::FrameAt<'_>) -> Option<ClassId> {
        if state.closure().is_some() {
            return self.closure_scope(state.closure());
        }
        match state.function() {
            Some(function) => self.method_class(function),
            None => state.scope(),
        }
    }

    /// The classes a `self`, a `parent` and a `static` in a type checked
    /// in `frame` name: the frame's scope and its called scope, which
    /// is what `zend_check_type_slow` resolves them against for an
    /// argument's `RECV` and for a return.
    pub(crate) fn frame_scopes(&self, frame: FrameRef) -> crate::api::coerce::Scopes {
        let own = self.frame_scope(frame);
        let called = self
            .vm
            .at(frame)
            .and_then(crate::exec::Header::called_scope);
        crate::api::coerce::Scopes {
            own,
            called: called.or(own),
            callables: Deprecations::Raised,
        }
    }

    /// A unit's load, the declarations php binds while the file
    /// compiles made before its main op array runs, each told to the
    /// observers as it is made, in php's order: where opcache serves the
    /// unit, as `zend_accel_load_script` copies the cached file's tables
    /// (`Z/ext/opcache/zend_accelerator_util_funcs.c:392-410`), every
    /// function, then every class, then the delayed early bindings;
    /// where it does not, as php's compiler declares each as it finishes
    /// it (`Z/Zend/zend_compile.c:8627`, `:9386`), in the order the file
    /// was compiled ([`Unit::top_level`]), the classes no top-level
    /// statement declares taking their place among the classes. A
    /// declaration that ends the request ends the load there, so what
    /// php declared before it was declared and told, and nothing after
    /// it: false says so.
    pub(crate) fn declare_unit(&mut self, unit: &PublishedUnit, load: &mut ClassLoad) -> bool {
        if load.served == Some(AccelServed::Compiled)
            && let Some(at) = (0..unit.functions.len()).find(|&at| self.redeclares(unit, at))
        {
            // The miss's compile meets the name first, telling nothing.
            return self.declare_function(unit, at).is_some();
        }
        if load.accelerated() {
            for at in 0..unit.functions.len() {
                if !self.declare_function_told(unit, at) {
                    return false;
                }
            }
        } else {
            for declared in &unit.top_level {
                let declared = match *declared {
                    TopLevel::Function(at) => {
                        self.declare_function_told(unit, usize::try_from(at).unwrap_or(usize::MAX))
                    }
                    TopLevel::Class(at) => self.declare_classes_through(
                        unit,
                        load,
                        Some(usize::try_from(at).unwrap_or(usize::MAX)),
                    ),
                };
                if !declared {
                    return false;
                }
            }
        }
        self.declare_classes_through(unit, load, None) && self.declare_delayed(unit, load)
    }

    /// Whether the function at `at` of `unit` names one the request
    /// declared already, which [`Request::declare_function`] refuses.
    fn redeclares(&self, unit: &PublishedUnit, at: usize) -> bool {
        let body = exec::Body::Function(u32::try_from(at).unwrap_or(u32::MAX));
        unit.functions.get(at).is_some_and(|(decl, _)| {
            !self.preloaded_function(unit, body)
                && self.function_named((&decl.name).into()).is_some()
        })
    }

    /// [`Request::declare_function`] for the function at `at` of `unit`,
    /// told to the observers where it was declared: false where the
    /// request ended.
    fn declare_function_told(&mut self, unit: &PublishedUnit, at: usize) -> bool {
        match self.declare_function(unit, at) {
            Some(true) => {
                crate::observer::function_declared_at_load(self, unit, at);
                true
            }
            Some(false) => true,
            None => false,
        }
    }

    /// Every function `unit` declares at its top level, as
    /// [`Request::declare_function`] declares each, for a test that
    /// serves a unit's frame by hand: false where the request ended.
    #[cfg(test)]
    pub(crate) fn declare_functions(&mut self, unit: &PublishedUnit) -> bool {
        (0..unit.functions.len()).all(|at| self.declare_function(unit, at).is_some())
    }

    /// `zend_hash_add_ptr(CG(function_table), …)` for the function at
    /// `at` of those the unit declares at its top level, before its
    /// main op array runs: php binds one at compile time and the
    /// compiler emitted no opline for it, so the request's table takes
    /// it here. The compiler refused a name the natives have or the file
    /// declares twice; a name another file of the request declared is
    /// `do_bind_function_error` at `E_COMPILE_ERROR`, which names the
    /// earlier declaration's file and line and ends the request before
    /// the frame is pushed, so nothing of a file that redeclares runs:
    /// none says so, and otherwise whether the function was declared,
    /// which one the preload holds, or a place the unit has no function
    /// at, is not.
    fn declare_function(&mut self, unit: &PublishedUnit, at: usize) -> Option<bool> {
        let Some((decl, _)) = unit.functions.get(at) else {
            return Some(false);
        };
        let body = exec::Body::Function(u32::try_from(at).unwrap_or(u32::MAX));
        if self.preloaded_function(unit, body) {
            return Some(false);
        }
        if let Some(earlier) = self.function_named((&decl.name).into()) {
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
            return None;
        }
        let Some(declared) = Function::local(self.user_functions.len()) else {
            return Some(false);
        };
        self.user_functions.push(UserFunction {
            unit: PublishedUnit::clone(unit),
            body,
            member: None,
            flags: crate::attributes::Flags::of(&decl.attributes),
            shape: FrameShape::of(decl, exec::body_of(unit, body)),
            record: UserFunction::record(unit, body, &decl.name.0),
        });
        self.functions.push(&decl.name.0, declared);
        Some(true)
    }

    /// `zend_hash_find(EG(class_table), lcname)`: the class the table
    /// holds under `name`, whatever its case ([`class_in`]).
    pub(crate) fn class_named(&self, name: Bare<'_>) -> Option<ClassId> {
        class_in(&self.shared, &self.declared, &self.aliases, name.bytes())
    }

    /// The request's strings, which the units it compiles for itself
    /// intern into ([`super::memory::RequestStrings`]), beside its
    /// classes as the compiler asks after one
    /// ([`php_compile::RequestClasses`]), and the arena its compiles
    /// parse into, borrowed apart so all three are handed to one compile.
    pub(crate) fn compiling<R>(
        &mut self,
        compile: impl FnOnce(
            &mut dyn Intern,
            php_compile::RequestClasses<'_>,
            &mut php_compile::syntax::Arena,
        ) -> R,
    ) -> R {
        let (shared, declared, aliases) = (&self.shared, &self.declared, &self.aliases);
        let holds = |name: &[u8]| class_in(shared, declared, aliases, name).is_some();
        let mut strings = super::memory::RequestStrings {
            shared,
            heap: &mut self.heap,
        };
        compile(
            &mut strings,
            php_compile::RequestClasses::new(&holds),
            &mut self.compile_arena,
        )
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
        if let Some(native) = self.native_class_at(class) {
            return self.shared.classes.get(native);
        }
        self.declared_row(class)?.decl()
    }

    /// Where an engine's class stands among the engine's; none for any
    /// other class.
    pub(crate) fn native_class_at(&self, class: ClassId) -> Option<usize> {
        usize::try_from(class.published_number()?)
            .ok()
            .filter(|at| *at < self.shared.classes.len())
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
        let passed = self
            .ancestors(class)
            .map_while(|parent| self.class_decl(parent).map(|decl| decl.flags));
        for passed in passed {
            flags.not_serializable |= passed.not_serializable;
            if passed.dynamic_properties == DynamicProperties::Allowed
                && flags.dynamic_properties == DynamicProperties::Deprecated
            {
                flags.dynamic_properties = DynamicProperties::Allowed;
            }
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

    /// `ZEND_FLF_FUNC(opline)` and `ZEND_FLF_HANDLER(opline)`: the native
    /// the engine's frameless table holds at `at`, a `FRAMELESS_ICALL_*`'s
    /// `extended_value`, and its entry, the interception's where an
    /// extension intercepts it: a frameless row names a native function,
    /// never a method, a trampoline or an enum's, so its entry is its own
    /// row of the entries, as [`Request::serve_native`] finds it; none
    /// past the table, which a unit this engine compiled never carries.
    pub(crate) fn frameless_entry(&self, at: u32) -> Option<(Function, crate::api::abi::Entry)> {
        let row = self.shared.frameless.get(at as usize)?;
        let entry = self.shared.entries.get(row.function as usize)?;
        Some((Function(row.function), *entry))
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
        let natives = u32::try_from(self.shared.classes.len()).unwrap_or(u32::MAX);
        (0..natives)
            .map(ClassId)
            .chain(self.declared.iter().map(|named| named.class))
            .collect()
    }

    /// `get_defined_functions()` over `EG(function_table)`: `internal`,
    /// every native's key, and `user`, every user function's, each the
    /// lowered name the table is keyed by and in the order the table
    /// took them. A closure is no row, since php never adds one to the
    /// table; a method is none either, being its class's.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn defined_functions(&mut self) -> Result<Owner<PhpArray>, Thrown> {
        let made = self.defined_functions_made();
        adopted(made)
    }

    /// What [`defined_functions`](Self::defined_functions) makes, its one owner not yet named.
    pub(crate) fn defined_functions_made(&mut self) -> Result<PhpArray, Thrown> {
        let (mut internal, mut user) = (Vec::new(), Vec::new());
        for function in self.functions() {
            let list = if self.user_function(function).is_some() {
                &mut user
            } else {
                &mut internal
            };
            if let Some(decl) = self.function_decl(function) {
                list.push(decl.name.0.to_ascii_lowercase());
            }
        }
        let mut array = self.array()?;
        for (list, names) in [(&b"internal"[..], internal), (b"user", user)] {
            let mut rows = self.array()?;
            for name in names {
                let name = PhpString::new(self, &name)?;
                rows.append(self, name.into_value())?;
            }
            let key = PhpString::new(self, list)?.into_key();
            let rows = Owner::adopt(Value::array(rows));
            array.set(self, key, rows)?;
        }
        Ok(array)
    }

    /// `zend_register_class_alias_ex`'s `zend_hash_add` into
    /// `EG(class_table)`: `class` under `alias`, which the table finds in
    /// any case and lists lowered, and `false` where the table has that
    /// name already, a class's or another alias's, and nothing is added.
    /// The name is the caller's to have held to php's reserved names. A
    /// class of the program's given its alias is linked under it for the observers
    /// (`Z/Zend/zend_API.c:3633-3636`), and an extension's class is not,
    /// as php tells none of an alias registered at `MINIT`.
    pub(crate) fn class_alias(&mut self, class: ClassId, alias: Bare<'_>) -> bool {
        if self.class_named(alias).is_some() {
            return false;
        }
        let place = self.buckets.take();
        let lowered = alias.bytes().to_ascii_lowercase();
        self.aliases
            .push(alias.bytes(), (place, lowered.clone(), class));
        if crate::observer::classes_linked_observed(self) && !self.native_class(class) {
            crate::observer::class_linked(self, class, &lowered);
        }
        true
    }
}

/// The class `EG(class_table)` holds under `name`, any case: an engine's
/// class or a native class's alias, a class the request declared, then
/// an alias the request added; what [`Request::class_named`] answers.
/// Each is one lookup in its table's keys, as `zend_hash_find` over the
/// lowered name is, the case folded by the table and nothing lowered: a
/// class declared at `at` of the request's rows is the `ClassId` past
/// the engine's classes.
fn class_in(
    shared: &Shared,
    declared: &Keyed<Named>,
    aliases: &Keyed<preload::Alias>,
    name: &[u8],
) -> Option<ClassId> {
    shared.class_at(name).or_else(|| {
        declared
            .find(name)
            .map(|named| named.class)
            .or_else(|| aliases.find(name).map(|(_, _, class)| *class))
    })
}

/// The flags and the arity of the function at `at` of a unit's
/// `dynamic` table, one a `DECLARE_FUNCTION` binds or a closure is made
/// of.
fn dynamic_traits(unit: &Unit, at: u32) -> (crate::attributes::Flags, FrameShape) {
    usize::try_from(at)
        .ok()
        .and_then(|at| unit.dynamic.get(at))
        .map(|(decl, code)| {
            (
                crate::attributes::Flags::of(&decl.attributes),
                FrameShape::of(decl, Some(code)),
            )
        })
        .unwrap_or_default()
}

/// `zend_class_redeclaration_error`'s two wordings over the earlier
/// declaration: what it is and its own name, and its site when it was a
/// user class.
fn redeclaration(earlier: &ClassDecl) -> String {
    let what = php_link::object_type(earlier.kind);
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
