//! The properties of a request's objects: a property placed by
//! `zend_get_property_offset`, its visibility and the scope it is read
//! from, read and written through the declared slot, the dynamic table,
//! a hook or a magic accessor, held to its declared type, by reference
//! with the typed property source list, `isset()`, `unset()` and
//! `empty()`, the magic accessors' guards, the walk `foreach` and
//! `get_object_vars()` make, and `unserialize()`'s writes. The guards,
//! the source lists and the scope a write names are [`Properties`]'s,
//! whose fields no file but this one reads.

use php_heap::{Fatal, Heap, Obj, Ref, TableRow, Zval};
use php_link::Row;
use php_unit::decl::{PropertyDecl, Visibility};

use crate::api::abi::{PropertyRead, PropertyTest};
use crate::api::call::Function;
use crate::api::raising::{Level, Thrown, none_pending};
use crate::api::values::walk::{self, ObjectTable, Table};
use crate::api::values::{
    ClassId, Key, Owner, PhpArray, PhpObject, PhpRef, PhpString, TypeTag, Value, adopted,
};

use frames::Link;

use super::{DeclaredSlot, Hooks, PropertyInfo, Request, frames};

/// `IN_GET`, `IN_SET`, `IN_UNSET` and `IN_ISSET`
/// (`Z/Zend/zend_object_handlers.c:43`, over
/// `ZEND_GUARD_PROPERTY_*`): the bit each magic accessor takes in the
/// property guard while it serves one name on one object, so that a
/// `__get` reading the name it was called for finds the guard standing
/// and falls through to the `Undefined property` warning instead of
/// reaching itself.
pub(super) const IN_GET: u32 = 1 << 0;

const IN_SET: u32 = 1 << 1;

const IN_UNSET: u32 = 1 << 2;

const IN_ISSET: u32 = 1 << 3;

/// Why `zend_get_property_offset` answered its wrong offset for a name
/// no declared instance property holds.
#[derive(Clone, Copy)]
enum Unplaced {
    /// A name led by a NUL byte (`zend_bad_property_name`).
    BadName,
    /// A `static` property the scope may not reach
    /// (`zend_bad_property_access`).
    Denied(Visibility),
}

/// What a write to a typed declared slot reads off its
/// `zend_property_info`: the class that declared it, which the
/// `TypeError` of a value the type refuses and the `Error` of a write
/// to a readonly property both name; the declared type; and whether
/// php marked it `ZEND_ACC_READONLY`.
pub(crate) struct TypedProperty {
    pub(crate) declaring: ClassId,
    pub(crate) ty: php_unit::decl::Type,
    pub(crate) readonly: bool,
}

/// One row of an object's property table as [`Request::property_rows`]
/// reads it: the name, the value, and for a declared slot the class
/// that declared it and its visibility.
pub(crate) type PropertyRow = (Key, Value, Option<(ClassId, Visibility)>);

/// Where a walk over an object stands at a property: a declared slot,
/// the key of a bucket of the dynamic table, a hooked property's value
/// as its `get` hook answered it, or a row the class's `get_properties`
/// answered that the object holds nowhere of its own; each of the last
/// two one owner the walker's to give up.
#[derive(Clone, Copy)]
pub(crate) enum Walked {
    Slot(u32),
    Dynamic(php_heap::Key),
    Hooked(Value),
    Answered(Value),
}

/// The key of a row [`Request::object_walk`] found, as php's walk holds
/// it until `FE_FETCH` reads it where the loop keeps one
/// ([`Request::walked_key`]): one the walk holds an owner of, a hooked
/// iterator's `current_key` or a bucket's key, or a declared property's
/// name, which the table holds mangled where it is not public.
pub(crate) enum WalkKey {
    Held(Owner<Key>),
    Declared(Box<[u8]>, Visibility),
}

impl Walked {
    /// Whether the walk holds an owner of the value it found.
    pub(crate) fn owned(self) -> bool {
        matches!(self, Walked::Hooked(_) | Walked::Answered(_))
    }
}

/// One `zend_property_info *` of a reference's source list
/// (`Z/Zend/zend_types.h:626`): a typed property whose slot holds the
/// cell, named by the class that declared it, the name the `TypeError`
/// prints and the declared type the write is held to. php names the
/// declaration itself; the linked tables here are shared and immutable,
/// so the row carries what the check and the message read.
pub(crate) struct RefSource {
    /// The class that declared the property, which is the one php's
    /// `prop->ce->name` prints.
    pub(crate) declaring: ClassId,
    /// The property's name, unmangled, as php's
    /// `zend_get_unmangled_property_name` hands it over.
    pub(crate) name: Vec<u8>,
    /// `prop_info->type`.
    pub(crate) ty: php_unit::decl::Type,
}

/// The magic accessors' guards, the typed properties' source lists and
/// the scope a write names, whose fields no file but this one reads.
pub(super) struct Properties {
    /// `EG(fake_scope)`: the class a write that names its own scope
    /// stands in while it lasts, as `zend_update_property_ex(scope, ...)`
    /// sets it, which the property placement reads ahead of the executed
    /// scope (`get_fake_or_executed_scope`).
    fake_scope: Option<ClassId>,
    /// `zend_get_property_guard` (`Z/Zend/zend_object_handlers.c:866`),
    /// what stops a magic accessor reaching itself: one word per
    /// property name per object, carrying `IN_GET`, `IN_SET`,
    /// `IN_ISSET` or `IN_UNSET` while that accessor serves that name
    /// on that object. php keeps the word on the object, in the slot
    /// after its declared properties; an object here carries what the
    /// heap lays down and nothing more, so the words are the request's,
    /// named by `spl_object_id()` and the property's name. A row is
    /// dropped the moment its word empties, which is before the object
    /// can die, so an index the store gives out again is never one an
    /// old row still names.
    guards: Vec<((php_heap::ObjectId, Vec<u8>), u32)>,
    /// The recursion guards of objects whose class uses guards, one row
    /// per walk inside one object (`ZEND_GUARD_RECURSION_*` on the
    /// word `zend_get_recursion_guard` finds), named as [`Self::guards`]
    /// names its rows and dropped the same way, when the walk leaves.
    recursion_guards: Vec<(php_heap::ObjectId, crate::api::values::RecursionGuard)>,
    /// `zend_property_info_source_list` (`Z/Zend/zend_types.h:626`),
    /// one row per reference a typed property's slot holds: the word
    /// [`php_heap::Ref::set_sources`] carries names the row, and the row
    /// lists the properties that type the cell. php hangs the list off
    /// the cell itself; the cell is the heap's and a declared type is
    /// the engine's, so the list is here and the cell carries its name.
    /// A row is dropped by `zend_reference_destroy`, which reaches
    /// [`HeapHooks::free_reference`].
    ref_sources: Vec<(usize, Vec<RefSource>)>,
    /// The next word [`Request::ref_add_type_source`] hands out, never
    /// reused, so a cell freed and a cell laid down at its address are
    /// never one row.
    next_ref_source: usize,
    /// The `foreach` loops over objects whose class has a hooked
    /// property, each while it runs ([`Request::hooked_walk_start`]).
    pub(super) hooked_walks: Vec<super::hooks::HookedWalk>,
}

impl Default for Properties {
    fn default() -> Properties {
        Properties {
            fake_scope: None,
            guards: Vec::new(),
            recursion_guards: Vec::new(),
            ref_sources: Vec::new(),
            next_ref_source: 1,
            hooked_walks: Vec::new(),
        }
    }
}

impl Request {
    /// `EG(fake_scope) = scope` for the length of `f`, and the scope
    /// that stood put back after it, whatever `f` answers: the one form
    /// every save of php's fake scope takes, so `properties.fake_scope`
    /// is written nowhere else.
    pub(crate) fn in_fake_scope<R>(
        &mut self,
        scope: Option<ClassId>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let saved = std::mem::replace(&mut self.properties.fake_scope, scope);
        let out = f(self);
        self.properties.fake_scope = saved;
        out
    }

    /// `zend_free_source_list`: the row the cell's word names, dropped
    /// with the cell ([`HeapHooks::free_reference`]).
    pub(super) fn free_source_list(&mut self, word: usize) {
        self.properties.ref_sources.retain(|(at, _)| *at != word);
    }

    /// `zend_get_executed_scope` (`Z/Zend/zend_execute_API.c:712`): the
    /// chain walked from `EG(current_execute_data)` to the first link
    /// that is user code or a function with a scope, and that link's
    /// `func->common.scope`: a user method's class, a closure's bound
    /// scope, and none for a plain user function or the main op array.
    /// A native with no scope, `constant()` or `get_class()` called
    /// from a method, is walked past, so the scope a native reads is
    /// its caller's; a native method's is its own class.
    /// `get_fake_or_executed_scope`: the scope a property is placed and
    /// its visibility measured from, the scope a write named for itself
    /// ([`Request::write_property_as_declared`]) or the executed one.
    pub(crate) fn property_scope(&self) -> Option<ClassId> {
        self.properties.fake_scope.or_else(|| self.executed_scope())
    }

    /// Whether a write that names its own scope stands
    /// (`EG(fake_scope)`), which a property's placement reads ahead of
    /// the executed scope: what an inline path that reads a placement a
    /// frame's scope made tests first.
    pub(crate) fn fakes_scope(&self) -> bool {
        self.properties.fake_scope.is_some()
    }

    /// `zend_get_property_offset`'s answer a fetch may cache
    /// (`Z/Zend/zend_object_handlers.c:384-389`, `:450-461`), for objects
    /// of `object`'s class served by the standard handlers: the declared
    /// slot of `name` and whether it is typed, where the property has no
    /// hooks and the scope a property is placed from reaches it, and
    /// `ZEND_DYNAMIC_PROPERTY_OFFSET` for a name no class of the chain
    /// declares, an instance or a `static` property, private or not, and
    /// that leads with no NUL byte. None for every other name, which is
    /// placed on every pass: one the scope may not reach, a `static`
    /// one, whose notice each read raises, one a class's own handlers
    /// serve.
    ///
    /// ```php
    /// class P { private $p = "P"; function viaP($o) { return $o->p; } }
    /// class C extends P { public $p = "C"; protected $q = 1; function viaC($o) { return $o->p; } }
    /// $c = new C; $d = new C; $d->p = "D";
    /// foreach ([$c, $d, $c] as $o) echo $o->viaP($o), $o->viaC($o), $o->p, "|";
    /// try { echo $c->q; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $c->dyn = 1; unset($d->p);
    /// foreach ([$c, $d] as $o) { try { echo $o->viaC($o), "|"; } catch (Error $e) {} }
    /// echo "\n";
    /// ```
    /// ```expect
    /// PCC|PDD|PCC|Cannot access protected property C::$q
    ///
    /// Deprecated: Creation of dynamic property C::$dyn is deprecated in %s on line 7
    /// C|
    /// Warning: Undefined property: C::$p in %s on line 3
    /// |
    /// ```
    ///
    /// A dynamic property is read and written through the one lookup the
    /// cache leaves, and every pass sees the table as it stands: a name
    /// unset and laid down again goes to the end, a reference is read
    /// through and written through, a table an `(array)` cast shares is
    /// separated before a write, and a name the table no longer holds is
    /// the `__get`'s or the warning's again.
    ///
    /// ```php
    /// class M { public function __get($n) { return "get $n"; } }
    /// function f($o) { $r = []; for ($i = 0; $i < 3; $i++) { $r[] = $o->p; $o->p = $i; $o->p++; $o->p += 10; } return $r; }
    /// $o = new stdClass; $o->a = 0; $o->p = 'x';
    /// var_dump(f($o), $o);
    /// $a = (array) $o; $o->p = 'y'; unset($o->a); $o->a = 1;
    /// var_dump($a['p'], get_object_vars($o));
    /// $x = 5; $o->p = &$x; f($o); var_dump($x);
    /// $m = new M; $m->p = 1; var_dump(f($m)); unset($m->p); var_dump($m->p);
    /// $s = new stdClass; foreach ([1, 2] as $k) { $s->{"k$k"} = $k; foreach ($s as $n => $v) echo "$n=$v "; echo $s->k1, "\n"; }
    /// ```
    /// ```expect
    /// array(3) {
    ///   [0]=>
    ///   string(1) "x"
    ///   [1]=>
    ///   int(11)
    ///   [2]=>
    ///   int(12)
    /// }
    /// object(stdClass)#1 (2) {
    ///   ["a"]=>
    ///   int(0)
    ///   ["p"]=>
    ///   int(13)
    /// }
    /// int(13)
    /// array(2) {
    ///   ["p"]=>
    ///   string(1) "y"
    ///   ["a"]=>
    ///   int(1)
    /// }
    /// int(13)
    ///
    /// Deprecated: Creation of dynamic property M::$p is deprecated in %s on line 9
    /// array(3) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(11)
    ///   [2]=>
    ///   int(12)
    /// }
    /// string(5) "get p"
    /// k1=1 1
    /// k1=1 k2=2 1
    /// ```
    ///
    /// A name a class declares `static` is no dynamic property, so every
    /// read and write raises its notice, and a name a class without
    /// `#[AllowDynamicProperties]` has not got is deprecated where it is
    /// laid down, once:
    ///
    /// ```php
    /// class S { public static $s = 1; }
    /// class D {}
    /// function g($o) { for ($i = 0; $i < 2; $i++) { $o->s = $i; echo $o->s, "\n"; } }
    /// g(new S); g(new D);
    /// ```
    /// ```expect
    ///
    /// Notice: Accessing static property S::$s as non static in %s on line 4
    ///
    /// Deprecated: Creation of dynamic property S::$s is deprecated in %s on line 4
    ///
    /// Notice: Accessing static property S::$s as non static in %s on line 4
    /// 0
    ///
    /// Notice: Accessing static property S::$s as non static in %s on line 4
    ///
    /// Notice: Accessing static property S::$s as non static in %s on line 4
    /// 1
    ///
    /// Deprecated: Creation of dynamic property D::$s is deprecated in %s on line 4
    /// 0
    /// 1
    /// ```
    pub(crate) fn cacheable_offset(
        &self,
        object: PhpObject,
        name: &[u8],
    ) -> Option<crate::exec::PropertyOffset> {
        let class = object.class();
        if self.own_handlers(class).is_some() {
            return None;
        }
        if let Some(placed) = self.declared_slot(class, name) {
            let reached = self.hooked(class, name).is_none() && self.reaches(&placed);
            return reached.then_some(crate::exec::PropertyOffset::Declared {
                slot: placed.slot,
                typed: placed.typed,
            });
        }
        let declared = self.placed_named(class, name).is_some()
            || self.chain_declaration(class, name).is_some();
        (!declared && !Self::bad_property_name(name))
            .then_some(crate::exec::PropertyOffset::Dynamic)
    }

    /// `zend_update_property_ex(prop_info->ce, ...)`, what an engine or a
    /// native write names a property by: the write made in the scope of
    /// the class whose declaration the object's table holds for `name`,
    /// so a private property of a parent class, `Exception::$trace` on a
    /// `TypeError`, is its slot and not a dynamic property; a name no
    /// class declared is written as the executed scope writes it.
    ///
    /// ```php
    /// class C { public $p { get { return intdiv(1, 0); } } }
    /// try { (new C)->p; } catch (DivisionByZeroError $e) { var_dump(count($e->getTrace()), array_key_exists("\0Error\0trace", (array) $e)); }
    /// ```
    /// ```expect
    /// int(2)
    /// bool(true)
    /// ```
    pub(crate) fn write_property_as_declared(
        &mut self,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let table = self.object_handlers(object.heap());
        // A view of the name, whose bytes the owner the write takes keeps
        // alive until the write has read the scope off them.
        let key = *name;
        self.in_declared_scope(object.class(), key.as_bytes(), |req| {
            table.write_property(req, object, name, value)
        })
    }

    /// `zend_update_property_ex(scope, ...)`: the write through the
    /// object's `write_property` with `EG(fake_scope)` set to `scope`,
    /// `None` leaving the executed scope to decide, and put back after.
    pub(crate) fn write_property_in(
        &mut self,
        object: PhpObject,
        scope: Option<ClassId>,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let table = self.object_handlers(object.heap());
        self.in_fake_scope(scope, |req| table.write_property(req, object, name, value))
    }

    /// `f` called with `EG(fake_scope)` the class whose declaration
    /// `class`'s table holds for `name`, as `zend_update_property_ex(ce,
    /// ...)` makes a write, and put back after it; a name no class
    /// declared leaves the scope as it stands.
    pub(crate) fn in_declared_scope<R>(
        &mut self,
        class: ClassId,
        name: &[u8],
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let declaring = self
            .placed_named(class, name)
            .map(|(_, declaring, _)| declaring);
        let scope = declaring.or(self.properties.fake_scope);
        self.in_fake_scope(scope, f)
    }

    pub(crate) fn executed_scope(&self) -> Option<ClassId> {
        for link in self.chain() {
            let (function, closure, carried) = match link {
                Link::Frame(_, state) => (state.function(), state.closure(), state.scope()),
                Link::Call(call) => (
                    call.function(),
                    call.closure(),
                    self.is_parent_hook_trampoline(call.callee())
                        .then_some(call.called_scope())
                        .flatten(),
                ),
                Link::Fake(_) => continue,
                Link::Site(_) => return None,
            };
            let scope = self
                .closure_scope(closure)
                .or_else(|| self.method_class(function?))
                .or(carried);
            let user = function.is_some_and(|function| self.user_function(function).is_some());
            if scope.is_some() || user || matches!(link, Link::Frame(..)) {
                return scope;
            }
        }
        None
    }

    /// `EG(fake_scope) = class` for the length of `f`, and the scope
    /// that stood put back after it, so it cannot leak past the call:
    /// the scope a native asks the engine to measure visibility from as
    /// though `class` were executing, as `ReflectionClass::newInstance`
    /// asks the object's `get_constructor` (`Z/ext/reflection/php_reflection.c:5086-5088`)
    /// and `zend_update_property_ex` writes. `ReflectionClass::newInstance`
    /// on a class with a private constructor is the witness, in
    /// `ext/reflection`.
    pub fn with_fake_scope<R>(&mut self, class: ClassId, f: impl FnOnce(&mut Request) -> R) -> R {
        self.in_fake_scope(Some(class), f)
    }

    /// `ce->default_properties_count`: the declared instance properties
    /// of the class and of every class above it, each a slot on the
    /// object ([`Request::placed_properties`]).
    pub(super) fn class_slots(&self, class: ClassId) -> u32 {
        u32::try_from(self.placed_properties(class).len()).unwrap_or(u32::MAX)
    }

    /// The slot a declared instance property of `class` sits in,
    /// `zend_get_property_offset` without visibility yet; `None` for a
    /// name no class of the chain declares, which is a dynamic property.
    /// The last row of the name is the one `ce->properties_info` holds,
    /// since a class's own declaration is appended after a parent's
    /// private one of the same name and hides it.
    pub(super) fn prop_slot(&self, class: ClassId, name: &[u8]) -> Option<u32> {
        self.placed_named(class, name)
            .and_then(|(slot, _, _)| u32::try_from(slot).ok())
    }

    /// The placed row `ce->properties_info` holds for `name`, whatever
    /// the scope: the last of the name, a class's own declaration being
    /// appended after a parent's private one it hides.
    pub(super) fn placed_named(
        &self,
        class: ClassId,
        name: &[u8],
    ) -> Option<(usize, ClassId, &PropertyDecl)> {
        self.placed(class)
            .rev()
            .find(|(_, _, p)| &*p.name.0 == name)
    }

    /// The dynamic table's entry for `name`, a view, found by the name's
    /// hash as `zend_hash_find` finds it; none where the object has no
    /// table or the table no such key.
    pub(crate) fn dynamic_prop(o: Obj, name: &[u8]) -> Option<Value> {
        o.properties()?.str_find(name).map(Value::of_slot)
    }

    /// A property read, `zend_std_read_property` without the handler
    /// in the way: the declared slot, `None` while nothing wrote it, or
    /// the dynamic table's entry. A view, no owner taken.
    pub(crate) fn object_prop(&self, o: Obj, name: &[u8]) -> Option<Value> {
        if let Some(i) = self.prop_slot(ClassId(o.class()), name) {
            let z = o.slot(i);
            return (!z.is_undef()).then(|| Value::of_slot(z));
        }
        Self::dynamic_prop(o, name)
    }

    /// `zend_get_property_offset` over the placed instance properties:
    /// the slot `name` sits in, the class that declared what stands
    /// there, its visibility, its set visibility and whether it is
    /// typed; none for a name no class of the chain declares, which is
    /// a dynamic property. The row is the one the executed scope finds
    /// ([`Request::placed_row`]). A declaration that shadows a parent's
    /// private property of the name (`ZEND_ACC_CHANGED`) is, from that
    /// parent's scope, the parent's own private slot
    /// (`zend_get_parent_private_property`).
    ///
    /// ```php
    /// class A { private int $prop = 42;
    ///     function r() { return $this->prop; }
    ///     function op() { $this->prop += 1; return $this->prop; }
    ///     function ref() { $x =& $this->prop; unset($this->prop); $x = "foo"; var_dump($x); } }
    /// class B extends A { private $prop = "b"; }
    /// $b = new B;
    /// var_dump($b->r(), $b->op());
    /// $b->ref();
    /// ```
    /// ```expect
    /// int(42)
    /// int(43)
    /// string(3) "foo"
    /// ```
    pub(super) fn declared_slot(&self, class: ClassId, name: &[u8]) -> Option<DeclaredSlot> {
        let (at, declaring, p) = self.placed_row(class, name)?;
        let root = if p.visibility == Visibility::Protected
            || p.set_visibility == Some(Visibility::Protected)
        {
            self.property_root(declaring, name)
        } else {
            declaring
        };
        Some(DeclaredSlot {
            slot: u32::try_from(at).unwrap_or(u32::MAX),
            declaring,
            visibility: p.visibility,
            set_visibility: p.set_visibility,
            typed: p.ty.is_some(),
            readonly: p.is_readonly,
            root,
        })
    }

    /// `property_info->prototype->ce` for the instance property `name`
    /// that `declaring` declares: `do_inherit_property` hands a child
    /// the parent's prototype where the parent's declaration is not
    /// private, so the walk goes up the parents while each one that
    /// declares the name declares it so.
    fn property_root(&self, declaring: ClassId, name: &[u8]) -> ClassId {
        let mut root = declaring;
        for here in self.ancestors(declaring) {
            let own = self.class_decl(here).and_then(|decl| {
                decl.properties
                    .iter()
                    .find(|p| &*p.name.0 == name)
                    .map(|p| p.visibility)
            });
            match own {
                Some(Visibility::Private) => break,
                Some(_) => root = here,
                None => {}
            }
        }
        root
    }

    /// `zend_get_property_offset`'s access rule over the row
    /// [`Request::declared_slot`] placed: public, the scope the declaring
    /// class, or, for a protected property, a scope related to its
    /// prototype's class (`is_protected_compatible_scope`), so two
    /// classes that each redeclare a parent's property reach each other's.
    ///
    /// ```php
    /// abstract class P { abstract protected $foo { get; } }
    /// class C1 extends P { protected $foo = 1; }
    /// class C2 extends P { protected $foo = 2; static function foo($c) { return $c->foo; } }
    /// var_dump(C2::foo(new C2), C2::foo(new C1));
    /// ```
    /// ```expect
    /// int(2)
    /// int(1)
    /// ```
    pub(super) fn reaches(&self, placed: &DeclaredSlot) -> bool {
        let scope = self.property_scope();
        match placed.visibility {
            Visibility::Public => true,
            _ if scope == Some(placed.declaring) => true,
            Visibility::Private => false,
            Visibility::Protected => self.check_protected(placed.root, scope),
        }
    }

    /// `zend_get_property_offset` where the row `ce->properties_info`
    /// holds for `name` is a `static` property, which the placed
    /// instance rows never are: `None` where the row is not one, and
    /// otherwise the rule over it, `Err` with its visibility where the
    /// scope may not reach it (`zend_bad_property_access`, the caller's
    /// to raise unless it places silently), and `Ok` where it may, the
    /// notice raised unless `silent` and the name then a dynamic
    /// property. The row is the class's own declaration, else the
    /// nearest parent's, a private one included, and under
    /// `ZEND_ACC_CHANGED` the scope's own private `static` where the
    /// scope is a parent of `class` that declares one.
    ///
    /// ```php
    /// class A { private static $a = "a"; protected static $b = "b"; public static $c = "c";
    ///     function f() { var_dump($this->a, $this->b, $this->c); } }
    /// class B extends A {}
    /// (new B)->f();
    /// try { var_dump((new B)->b); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    ///
    /// Notice: Accessing static property B::$a as non static in %s on line 3
    ///
    /// Warning: Undefined property: B::$a in %s on line 3
    ///
    /// Notice: Accessing static property B::$b as non static in %s on line 3
    ///
    /// Warning: Undefined property: B::$b in %s on line 3
    ///
    /// Notice: Accessing static property B::$c as non static in %s on line 3
    ///
    /// Warning: Undefined property: B::$c in %s on line 3
    /// NULL
    /// NULL
    /// NULL
    /// Cannot access protected property B::$b
    /// ```
    pub(crate) fn static_as_instance(
        &mut self,
        class: ClassId,
        name: &[u8],
        silent: bool,
    ) -> Option<Result<(), Visibility>> {
        let (declaring, p) = self.chain_declaration(class, name)?;
        let (is_instance, visibility) = (p.is_instance, p.visibility);
        let scope = self.property_scope();
        if scope != Some(declaring) {
            // `ZEND_ACC_CHANGED`: the scope's own private `static` wins
            // where the row is a `static` too.
            let own = scope.is_some_and(|scope| {
                scope != class
                    && self.instance_of(class, scope)
                    && self.class_decl(scope).is_some_and(|decl| {
                        decl.properties.iter().any(|p| {
                            !p.is_instance
                                && p.visibility == Visibility::Private
                                && &*p.name.0 == name
                        })
                    })
            });
            match visibility {
                _ if is_instance => return None,
                _ if own => {}
                Visibility::Public => {}
                Visibility::Private if declaring != class => return None,
                Visibility::Protected if self.check_protected(declaring, scope) => {}
                Visibility::Private | Visibility::Protected => return Some(Err(visibility)),
            }
        } else if is_instance {
            return None;
        }
        if !silent {
            let shown = self.class_name(class);
            crate::api::raising::notice(
                self,
                &format!(
                    "Accessing static property {shown}::${} as non static",
                    String::from_utf8_lossy(name)
                ),
            );
        }
        Some(Ok(()))
    }

    /// The declaration of `name`, an instance or a `static` property, of
    /// the nearest class of `class`'s chain that declares one, beside
    /// that class: what `ce->properties_info` finds where no placed row
    /// does.
    fn chain_declaration(&self, class: ClassId, name: &[u8]) -> Option<(ClassId, &PropertyDecl)> {
        self.lineage(class).find_map(|here| {
            let p = self
                .class_decl(here)?
                .properties
                .iter()
                .find(|p| &*p.name.0 == name)?;
            Some((here, p))
        })
    }

    /// `ZEND_WRONG_PROPERTY_OFFSET` for a name no instance row placed:
    /// a name led by a NUL byte, or a `static` property the scope may
    /// not reach ([`Request::static_as_instance`], whose notice for one
    /// it may reach is raised here unless `silent`). `None` is a
    /// dynamic property.
    fn unplaced(&mut self, class: ClassId, name: &[u8], silent: bool) -> Option<Unplaced> {
        if Self::bad_property_name(name) {
            return Some(Unplaced::BadName);
        }
        match self.static_as_instance(class, name, silent) {
            Some(Err(visibility)) => Some(Unplaced::Denied(visibility)),
            _ => None,
        }
    }

    /// The `Error` for what [`Request::unplaced`] refused.
    fn unplaced_error(&mut self, class: ClassId, name: &[u8], miss: Unplaced) -> Thrown {
        match miss {
            Unplaced::BadName => self.bad_property_name_error(),
            Unplaced::Denied(visibility) => self.inaccessible_property(class, visibility, name),
        }
    }

    /// `zend_get_property_offset`'s choice of `property_info` for `name`
    /// on an object of `class` under the executed scope, as the placed
    /// row it names: the row `ce->properties_info` holds, the class's
    /// own declaration over a parent's private one; under
    /// `ZEND_ACC_CHANGED` the scope's own private property where the
    /// scope is a parent of `class` that declares one
    /// (`zend_get_parent_private_property`); and none where the row is
    /// a parent's private property the scope is not, which php reads as
    /// a dynamic property (`goto dynamic`).
    ///
    /// ```php
    /// class A { private $shadowed = 'A'; private $p = 'A::p';
    ///     function test() { foreach ($this as $k => $v) { echo "$k => $v\n"; } var_dump($this->p); } }
    /// #[AllowDynamicProperties]
    /// class B extends A { public $p = 'B::p'; }
    /// $b = new B;
    /// $b->shadowed = 'Global';
    /// $b->test();
    /// var_dump($b->p);
    /// ```
    /// ```expect
    /// shadowed => A
    /// p => A::p
    /// string(4) "A::p"
    /// string(4) "B::p"
    /// ```
    /// `zend_get_parent_private_property`: the executed scope, where it
    /// is a parent of `class` that declares `name` as a private instance
    /// property of its own.
    pub(crate) fn parent_private(&self, class: ClassId, name: &[u8]) -> Option<ClassId> {
        self.property_scope().filter(|&scope| {
            scope != class
                && self.instance_of(class, scope)
                && self.class_decl(scope).is_some_and(|decl| {
                    decl.properties.iter().any(|p| {
                        p.is_instance && p.visibility == Visibility::Private && &*p.name.0 == name
                    })
                })
        })
    }

    fn placed_row(&self, class: ClassId, name: &[u8]) -> Option<(usize, ClassId, &PropertyDecl)> {
        let found = self.placed_named(class, name)?;
        let (_, declaring, p) = found;
        if self.property_scope() != Some(declaring) {
            if let Some(own) = self.parent_private(class, name) {
                return Some(
                    self.placed(class)
                        .find(|row| row.1 == own && &*row.2.name.0 == name)
                        .unwrap_or(found),
                );
            } else if p.visibility == Visibility::Private && declaring != class {
                return None;
            }
        }
        Some(found)
    }

    /// `zend_asymmetric_property_has_set_access`
    /// (`Z/Zend/zend_object_handlers.c:590`), the one rule a plain and a
    /// hooked property's set visibility are held to: whether the
    /// executed scope may write a property `declaring` declares, whose
    /// prototype's class is `root` (`prop_info->prototype->ce`), and
    /// whose set visibility is `set`. The declaring class writes its
    /// own; a `protected(set)` one is a scope's related to the
    /// prototype's class too (`is_protected_compatible_scope`), so a
    /// sibling that redeclares nothing writes a redeclaration of their
    /// parent's property. A `public(set)` never reaches here, since
    /// `zend_declare_typed_property` drops a set visibility the read
    /// visibility already equals and refuses one weaker than it.
    ///
    /// ```php
    /// class P { public protected(set) int $x = 0; }
    /// class A extends P { public protected(set) int $x = 0; }
    /// class B extends P { function poke(A $a) { $a->x = 5; return $a->x; } }
    /// try { var_dump((new B)->poke(new A)); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
    /// class Q { public protected(set) int $y { get => $this->y; set => $this->y = $value; } }
    /// class R extends Q { public protected(set) int $y { get => $this->y; set => $this->y = $value * 2; } }
    /// class S extends Q { function poke(R $r) { $r->y = 5; return $r->y; } }
    /// var_dump((new S)->poke(new R));
    /// class U { private int $z = 0; }
    /// class V extends U { public protected(set) int $z = 0; }
    /// class W extends V {}
    /// class X extends U { function poke(V $v) { $v->z = 1; } }
    /// try { (new X)->poke(new W); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// class SP { public protected(set) static int $s = 0; }
    /// class SA extends SP { public protected(set) static int $s = 0; }
    /// class SB extends SP { static function poke() { SA::$s = 7; return SA::$s; } }
    /// var_dump(SB::poke());
    /// ```
    /// ```expect
    /// int(5)
    /// int(10)
    /// Cannot modify protected(set) property V::$z from scope X
    /// int(7)
    /// ```
    pub(super) fn has_set_access(
        &self,
        declaring: ClassId,
        root: ClassId,
        set: Visibility,
    ) -> bool {
        let scope = self.property_scope();
        scope == Some(declaring)
            || (set == Visibility::Protected && self.check_protected(root, scope))
    }

    /// Whether the property `info` names has a set visibility the
    /// executed scope lacks.
    pub(crate) fn lacks_set_access(&self, info: PropertyInfo) -> bool {
        self.property_decl(info).is_some_and(|property| {
            property.set_visibility.is_some_and(|set| {
                let root = self.property_root(info.declaring, &property.name.0);
                !self.has_set_access(info.declaring, root, set)
            })
        })
    }

    /// `prop_info->flags & ZEND_ACC_PPP_SET_MASK &&
    /// !zend_asymmetric_property_has_set_access(prop_info)`, the test the
    /// opcodes over a `static` property make of it once it is fetched,
    /// with php's `Error` for `operation`: `modify` for
    /// `ASSIGN_STATIC_PROP` and `indirectly modify` for the fetches and
    /// the read-modify-write opcodes.
    ///
    /// ```php
    /// class C { public private(set) static int $p = 1; public private(set) static array $a = [];
    ///     static function set() { self::$p = 5; self::$a[] = 1; } }
    /// try { C::$p = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { C::$p++; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { C::$p .= "1"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { C::$a[] = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $r = &C::$p; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// C::set();
    /// var_dump(C::$p, C::$a);
    /// ```
    /// ```expect
    /// Cannot modify private(set) property C::$p from global scope
    /// Cannot indirectly modify private(set) property C::$p from global scope
    /// Cannot indirectly modify private(set) property C::$p from global scope
    /// Cannot indirectly modify private(set) property C::$a from global scope
    /// Cannot indirectly modify private(set) property C::$p from global scope
    /// int(5)
    /// array(1) {
    ///   [0]=>
    ///   int(1)
    /// }
    /// ```
    pub(crate) fn set_refused(&mut self, info: PropertyInfo, operation: &str) -> Option<Thrown> {
        if !self.lacks_set_access(info) {
            return None;
        }
        let property = self.property_decl(info)?;
        let set = property.set_visibility?;
        let (readonly, name) = (property.is_readonly, property.name.0.clone());
        Some(self.set_inaccessible_property(info.declaring, set, readonly, &name, operation))
    }

    /// `zend_asymmetric_visibility_property_modification_error`
    /// (`Z/Zend/zend_execute.c:931`): php's `Error` for a write the set
    /// visibility refuses, `operation` the word the site does
    /// (`modify`, `indirectly modify`, `unset`). php names the called
    /// scope here, not the executed one; the two differ only where a
    /// method is reached through a class that inherited it, and the
    /// linker that inherits a method arrives with the classes of design
    /// §11 days 8-14. A readonly property's `protected(set)` is named
    /// with its `readonly`, which is the set visibility
    /// `zend_declare_typed_property` gives every public readonly one:
    ///
    /// ```php
    /// class A { public readonly int $x; function __construct(bool $set) { if ($set) { $this->x = 1; } } }
    /// $a = new A(false);
    /// try { $a->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { unset($a->x); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $b = new A(true);
    /// try { $b->x = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { unset($b->x); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot modify protected(set) readonly property A::$x from global scope
    /// Cannot unset protected(set) readonly property A::$x from global scope
    /// Cannot modify readonly property A::$x
    /// Cannot unset readonly property A::$x
    /// ```
    ///
    /// `__clone` may unset and set one again, php's `IS_PROP_REINITABLE`:
    ///
    /// ```php
    /// class F { function __construct(public readonly stdClass $b) {} function __clone() { unset($this->b); $this->b = new stdClass; } }
    /// $f = new F(new stdClass);
    /// $g = clone $f;
    /// var_dump($f->b === $g->b);
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    pub(super) fn set_inaccessible_property(
        &mut self,
        class: ClassId,
        set: Visibility,
        readonly: bool,
        name: &[u8],
        operation: &str,
    ) -> Thrown {
        let word = match set {
            Visibility::Private => "private(set)",
            _ if readonly => "protected(set) readonly",
            _ => "protected(set)",
        };
        let from = match self.executed_scope() {
            Some(scope) => format!("scope {}", self.class_name(scope)),
            None => String::from("global scope"),
        };
        let class = self.class_name(class);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Cannot {operation} {word} property {class}::${shown} from {from}"),
        )
    }

    /// `zend_verify_property_access`: whether the executed scope may
    /// reach a property of `class` declared with `visibility`.
    pub(super) fn may_access(&self, class: ClassId, visibility: Visibility) -> bool {
        self.may_access_from(class, visibility, self.executed_scope())
    }

    /// [`Request::may_access`] from a scope named rather than the one
    /// executing: what `zend_verify_const_access` checks a class
    /// constant against inside a constant expression, the scope it is
    /// evaluated in.
    pub(super) fn may_access_from(
        &self,
        class: ClassId,
        visibility: Visibility,
        scope: Option<ClassId>,
    ) -> bool {
        match visibility {
            Visibility::Public => true,
            _ if scope == Some(class) => true,
            Visibility::Private => false,
            Visibility::Protected => self.check_protected(class, scope),
        }
    }

    /// `zend_hash_find_ptr(&ce->properties_info, name)`: one lookup in
    /// the class's linked record ([`Request::linked_class`]), whose
    /// property rows are `properties_info` as `zend_do_inheritance` left
    /// it, the class's own declarations first and a parent's appended
    /// where the class declares none of the name. The answer names the
    /// class that declared it, the property's `ce`, which for a `static`
    /// property is the class whose slot the value sits in (php's
    /// `IS_INDIRECT` into the parent's table) and the scope its
    /// visibility is measured against. An instance property is found
    /// here too, as php finds one in the same table and rejects it after
    /// the access check.
    ///
    /// A static property inherited through two levels shares its slot
    /// with the class that declared it, one the child redeclares is its
    /// own, typed as the child declares it, and an instance property or
    /// a grandparent's private one is found and refused:
    ///
    /// ```php
    /// class G { public static $shared = "G"; public static int $s = 1; private static $hidden = "G"; public $i = "G"; }
    /// class P extends G {}
    /// class C extends P { public static int $s = 2; public $i = "C"; }
    /// C::$shared = "set through C";
    /// echo G::$shared, " ", C::$s, " ", G::$s, "\n";
    /// try { C::$s = "x"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { echo C::$i; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { echo C::$hidden; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump(isset(C::$hidden), (new C)->i);
    /// ```
    /// ```expect
    /// set through C 2 1
    /// Cannot assign string to property C::$s of type int
    /// Access to undeclared static property C::$i
    /// Cannot access private property C::$hidden
    /// bool(false)
    /// string(1) "C"
    /// ```
    pub(crate) fn property_info_of(
        &self,
        class: ClassId,
        name: &[u8],
    ) -> Option<(ClassId, &PropertyDecl)> {
        let info = self.property_info(class, name)?;
        Some((info.declaring, self.property_decl(info)?))
    }

    /// [`Request::property_info_of`]'s row alone, the row a caller
    /// keeps across what writes the request and reads the declaration
    /// through again ([`Request::property_decl`]).
    pub(crate) fn property_info(&self, class: ClassId, name: &[u8]) -> Option<PropertyInfo> {
        let row = |row: &Row<ClassId>| PropertyInfo {
            declaring: row.holder_in(class),
            at: row.at,
        };
        let found = self.linked_class(class)?.properties.find(name, |at| {
            self.property_decl(row(at))
                .map_or(&[][..], |property| &property.name.0)
        })?;
        Some(row(found))
    }

    /// `ZEND_REF_ADD_TYPE_SOURCE` (`Z/Zend/zend_types.h:1082`): the
    /// declared type of a property whose slot holds `cell` is put on the
    /// cell, so every write through any name bound to it is held to the
    /// type as well ([`Request::assign_to_reference`]). A cell typed by
    /// nothing yet takes a row of its own, whose word the cell carries.
    pub(crate) fn ref_add_type_source(&mut self, cell: Ref, source: RefSource) {
        let word = cell.sources();
        if word != 0
            && let Some((_, sources)) = self
                .properties
                .ref_sources
                .iter_mut()
                .find(|(at, _)| *at == word)
        {
            sources.push(source);
            return;
        }
        let word = self.properties.next_ref_source;
        self.properties.next_ref_source = self.properties.next_ref_source.saturating_add(1);
        self.properties.ref_sources.push((word, vec![source]));
        cell.set_sources(word);
    }

    /// `ZEND_REF_DEL_TYPE_SOURCE`: the property is no longer one of the
    /// cell's sources, because its slot now holds something else. The
    /// row goes with the last source, and the cell's word with it.
    pub(crate) fn ref_del_type_source(&mut self, cell: Ref, declaring: ClassId, name: &[u8]) {
        let word = cell.sources();
        if word == 0 {
            return;
        }
        let Some(at) = self
            .properties
            .ref_sources
            .iter()
            .position(|(held, _)| *held == word)
        else {
            return;
        };
        if let Some((_, sources)) = self.properties.ref_sources.get_mut(at) {
            if let Some(i) = sources
                .iter()
                .position(|s| s.declaring == declaring && s.name == name)
            {
                sources.remove(i);
            }
            if !sources.is_empty() {
                return;
            }
        }
        self.properties.ref_sources.remove(at);
        cell.set_sources(0);
    }

    /// `ZEND_REF_DEL_TYPE_SOURCE` over the property whose slot `slot`
    /// of `object` is (`zend_get_property_info_for_slot`): the cell the
    /// slot held is typed by that property no longer, because the slot
    /// is unset, reset or freed. A slot of an untyped property typed
    /// nothing.
    pub(crate) fn slot_source_gone(&mut self, object: PhpObject, slot: u32, cell: Ref) {
        let typed = self
            .placed_property(object.class(), slot)
            .filter(|(_, p)| p.ty.is_some())
            .map(|(declaring, p)| (declaring, p.name.0.clone()));
        if let Some((declaring, name)) = typed {
            self.ref_del_type_source(cell, declaring, &name);
        }
    }

    /// `zend_object_dtor_property` then `ZVAL_UNDEF`: what the slot held
    /// given up, a cell it held typed by the slot's property no longer
    /// ([`Request::slot_source_gone`]), and the slot left empty. What a
    /// lazy object does to its slots goes through here: a reset, the
    /// proxy's slots once its instance stands, and an initializer's
    /// revert, which types the cell it puts back again.
    ///
    /// ```php
    /// class C { public ?C $a; public $c; public function __construct() { $this->a = null; } }
    /// $r = new ReflectionClass(C::class);
    /// $objs = ['ghost' => $r->newLazyGhost(function ($o) { $o->__construct(); }),
    ///     'proxy' => $r->newLazyProxy(fn () => new C),
    ///     'failed' => $r->newLazyGhost(function ($o) { throw new Exception('no'); })];
    /// foreach ($objs as $k => $obj) {
    ///     $r->getProperty('a')->setRawValueWithoutLazyInitialization($obj, null);
    ///     $ref = &$obj->a;
    ///     try { $obj->c; } catch (Exception $e) { echo "$k: ", $e->getMessage(), "\n"; }
    ///     try { $ref = 1; echo "$k: bound to nothing typed\n"; } catch (TypeError $e) { echo "$k: ", $e->getMessage(), "\n"; }
    ///     unset($ref);
    /// }
    /// ```
    /// ```expect
    /// ghost: Cannot assign int to reference held by property C::$a of type ?C
    /// proxy: bound to nothing typed
    /// failed: no
    /// failed: Cannot assign int to reference held by property C::$a of type ?C
    /// ```
    pub(crate) fn dtor_property(&mut self, object: PhpObject, slot: u32) {
        let o = object.heap();
        if let Some(cell) = o.slot(slot).as_reference() {
            self.slot_source_gone(object, slot, cell);
        }
        if !o.slot(slot).is_undef() {
            Heap::object_set_slot(&mut Hooks(self), o, slot, Zval::UNDEF);
        }
    }

    /// `zend_handle_fetch_obj_flags` under `ZEND_FETCH_REF`
    /// (`Z/Zend/zend_execute.c:3470`) over a typed property's slot `at`:
    /// a slot nothing has written refused where the type takes no `null`
    /// ([`Request::refuse_unwritten_ref`]), and a slot holding no cell yet
    /// made one, `null` where nothing was written, with the property as
    /// its type source, so every write through a name bound to it is held
    /// to the type ([`Request::assign_to_reference`]). `FETCH_OBJ_W`'s
    /// handler takes the place so for `&$o->p`, and
    /// [`PhpObject::typed_row_ref`] for a row of the property table.
    pub(crate) fn fetch_typed_slot_ref(
        &mut self,
        at: php_heap::ZvalRef,
        typed: &TypedProperty,
        name: &[u8],
    ) -> Result<(), Thrown> {
        let held = Value::of_slot(at.read());
        self.refuse_unwritten_ref(&held, typed, name)?;
        if held.as_php_ref().is_none() {
            let cell = self.reference(held.undef_as_null())?;
            at.write(cell.as_value().to_zval());
            let source = RefSource {
                declaring: typed.declaring,
                name: name.to_vec(),
                ty: typed.ty.clone(),
            };
            self.ref_add_type_source(cell.heap(), source);
        }
        Ok(())
    }

    /// `ZEND_FETCH_REF`'s refusal (`zend_handle_fetch_obj_flags`): a
    /// typed property nothing has written, `held` its slot, bound by `&`
    /// where its type takes no `null`, which php's `Error` names.
    pub(crate) fn refuse_unwritten_ref(
        &mut self,
        held: &Value,
        typed: &TypedProperty,
        name: &[u8],
    ) -> Result<(), Thrown> {
        if held.tag() != TypeTag::Undef || typed.ty.mask() & php_unit::decl::Type::Null.mask() != 0
        {
            return Ok(());
        }
        let class = self.class_name(typed.declaring);
        let shown = String::from_utf8_lossy(name);
        Err(crate::api::raising::throw_error(
            self,
            "Error",
            &format!(
                "Cannot access uninitialized non-nullable property {class}::${shown} by reference"
            ),
        ))
    }

    /// `zend_get_typed_property_info_for_slot`: the typed property whose
    /// slot `slot` of an object of `class` is, beside its unmangled name;
    /// none for an untyped one.
    pub(crate) fn slot_typed_property(
        &self,
        class: ClassId,
        slot: u32,
    ) -> Option<(TypedProperty, Vec<u8>)> {
        let (declaring, p) = self.placed_property(class, slot)?;
        let name = p.name.0.to_vec();
        let typed = TypedProperty {
            declaring,
            ty: p.ty.clone()?,
            readonly: p.is_readonly,
        };
        Some((typed, name))
    }

    /// `ZEND_REF_ADD_TYPE_SOURCE` over the property whose slot `slot` of
    /// `object` is (`zend_get_property_info_for_slot`): the cell the slot
    /// holds is typed by that property too, one more source beside the
    /// others, since another object's slot of the same property may
    /// hold it as well. A slot of an untyped property types nothing.
    pub(crate) fn slot_source_added(&mut self, object: PhpObject, slot: u32, cell: Ref) {
        let Some((declaring, p)) = self.placed_property(object.class(), slot) else {
            return;
        };
        if let Some(ty) = p.ty.clone() {
            let source = RefSource {
                declaring,
                name: p.name.0.to_vec(),
                ty,
            };
            self.ref_add_type_source(cell, source);
        }
    }

    /// `ZEND_REF_FOREACH_TYPE_SOURCES` as a slice: the properties whose
    /// slots hold this cell, empty for a cell no typed property's slot
    /// holds, which is `ZEND_REF_HAS_TYPE_SOURCES` reading false.
    pub(crate) fn ref_type_sources(&self, cell: Ref) -> &[RefSource] {
        let word = cell.sources();
        if word == 0 {
            return &[];
        }
        self.properties
            .ref_sources
            .iter()
            .find(|(at, _)| *at == word)
            .map_or(&[], |(_, sources)| sources.as_slice())
    }

    /// `zend_assign_to_typed_ref` (`Z/Zend/zend_execute.c:4070`): a
    /// write through a cell, held first to every type its sources
    /// declare ([`crate::ops::verify_ref_assignable`]) and coerced to
    /// what they agree on. A cell no typed property's slot holds is
    /// [`Request::reference_set`] and nothing else, which is what php's
    /// `zend_assign_to_variable` does where `ZEND_REF_HAS_TYPE_SOURCES`
    /// reads false. The value is given up with the `TypeError` and what
    /// the cell held stands, as php gives up its copy.
    pub(crate) fn assign_to_reference(
        &mut self,
        cell: Ref,
        value: Value,
        strict: bool,
    ) -> Result<(), Thrown> {
        if cell.sources() == 0 {
            self.reference_set(cell, value);
            return Ok(());
        }
        let verdict = crate::ops::verify_ref_assignable(self, cell, &value, strict);
        let value = crate::ops::landed(self, value, verdict)?;
        self.reference_set(cell, value);
        Ok(())
    }

    /// php's notice where a write fetch fell back to `__get` and what it
    /// answered is neither an object nor a reference: a write through the
    /// copy has no effect on anything (`zend_std_read_property`, `Indirect
    /// modification of overloaded property`), naming the object's class.
    ///
    /// A private property the scope may not reach is `__get`'s too (#513):
    /// an int it answers is the notice, and an object is written through.
    ///
    /// ```php
    /// class C { private $p = 1; private $q;
    ///     function __construct() { $this->q = new stdClass; }
    ///     function __get($n) { return $this->$n; } }
    /// $c = new C;
    /// try { $c->p->x = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $c->q->x = 3;
    /// var_dump($c);
    /// ```
    /// ```expect
    ///
    /// Notice: Indirect modification of overloaded property C::$p has no effect in %s on line 6
    /// Attempt to assign property "x" on int
    /// object(C)#1 (2) {
    ///   ["p":"C":private]=>
    ///   int(1)
    ///   ["q":"C":private]=>
    ///   object(stdClass)#2 (1) {
    ///     ["x"]=>
    ///     int(3)
    ///   }
    /// }
    /// ```
    fn overloaded_write_notice(
        &mut self,
        class: ClassId,
        name: &[u8],
        read: PropertyRead,
        got: &Value,
    ) {
        if matches!(read, PropertyRead::Write | PropertyRead::Unset)
            && got.as_php_ref().is_none()
            && got.as_object().is_none()
        {
            let class = self.class_name(class);
            let shown = String::from_utf8_lossy(name).into_owned();
            crate::api::raising::notice(
                self,
                &format!(
                    "Indirect modification of overloaded property {class}::${shown} has no effect"
                ),
            );
        }
    }

    /// Whether a write fetch over `name` falls back to `read_property`
    /// for the class's `__get` to answer, where php's
    /// `zend_std_get_property_ptr_ptr` answers `NULL` for it: a declared
    /// property the scope may not reach, a declared slot it reaches that
    /// an `unset` emptied, or a name the
    /// object holds no dynamic property for, on a class with a `__get`
    /// not already serving the name. A slot that is `IS_PROP_UNINIT` or
    /// waits on a lazy initializer is the placement's.
    ///
    /// ```php
    /// class M { private $data = ['a' => [1], 'o' => null];
    ///     function __construct() { $this->data['o'] = new stdClass; }
    ///     function __get($n) { echo "__get $n\n"; return $this->data[$n]; } }
    /// $m = new M;
    /// $m->a[] = 2;
    /// $m->o->x = 3;
    /// var_dump($m->a, $m->o);
    /// ```
    /// ```expect
    /// __get a
    ///
    /// Notice: Indirect modification of overloaded property M::$a has no effect in %s on line 6
    /// __get o
    /// __get a
    /// __get o
    /// array(1) {
    ///   [0]=>
    ///   int(1)
    /// }
    /// object(stdClass)#2 (1) {
    ///   ["x"]=>
    ///   int(3)
    /// }
    /// ```
    pub(crate) fn overloaded_fetch(&self, object: PhpObject, name: &[u8]) -> bool {
        let class = object.class();
        if !self.serves_accessor(class, b"__get") {
            return false;
        }
        if self.wrong_name(class, name) {
            return true;
        }
        if self.guard_word(object, name) & IN_GET != 0 {
            return false;
        }
        let o = object.heap();
        match self.declared_slot(class, name) {
            // A property the scope may not reach is placed silently where
            // the class has a `__get`, which answers for it.
            Some(placed) if !self.reaches(&placed) => true,
            Some(placed) => {
                o.slot(placed.slot).is_undef()
                    && !o.slot_uninit(placed.slot)
                    && !self.lazy_slot(o, placed.slot)
            }
            None => Self::dynamic_prop(o, name).is_none(),
        }
    }

    /// `zend_std_read_property`'s `/* magic isset */` arm under
    /// `BP_VAR_IS`: the `__isset` asked where its guard is down, `null`
    /// where it said no, and the `__get` served after a yes where its
    /// guard is down, what it answered held to the property's type
    /// ([`Request::hold_magic_get`]). `None` is neither accessor
    /// answering, and the read goes on to the warning's arm. The caller
    /// holds the object across the two.
    ///
    /// ```php
    /// class Test {
    ///     public function __isset($x) { $GLOBALS["obj"] = 24; return true; }
    ///     public function __get($x) { var_dump($this); return 42; }
    /// }
    /// $obj = new Test;
    /// $name = "foo";
    /// var_dump($obj->$name ?? 12);
    /// var_dump($obj);
    /// ```
    /// ```expect
    /// object(Test)#1 (0) {
    /// }
    /// int(42)
    /// int(24)
    /// ```
    fn isset_then_get(
        &mut self,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
        issetter: Function,
        getter: Option<Function>,
    ) -> Result<Option<Value>, Thrown> {
        if self.guard_word(object, name) & IN_ISSET == 0 {
            let answered = self.call_accessor(object, issetter, IN_ISSET, name, &[])?;
            let truthy = crate::api::coerce::truthy(self, &answered);
            self.release(answered);
            if !truthy? {
                return Ok(Some(Value::null()));
            }
        }
        let Some(getter) = getter.filter(|_| self.guard_word(object, name) & IN_GET == 0) else {
            return Ok(None);
        };
        let class = object.class();
        let got = self.call_accessor(object, getter, IN_GET, name, &[])?;
        self.overloaded_write_notice(class, name, read, &got);
        self.hold_magic_get(class, name, getter, got).map(Some)
    }

    /// `zend_verify_prop_assignable_by_ref_ex` under
    /// `ZEND_VERIFY_PROP_ASSIGNABLE_BY_REF_CONTEXT_MAGIC_GET`: what `__get`
    /// answered for a typed property an `unset` emptied, held to the
    /// property's type under `__get`'s own `strict_types`, converted by the
    /// weak rule where it can be, and php's `TypeError` naming `__get`
    /// where not. A name that is no typed declared property the scope
    /// reaches is the answer as it is.
    ///
    /// ```php
    /// class T { public int $i = 1; public int $u;
    ///     function __get($n) { echo "__get $n\n"; return $n === 'i' ? "42" : "x"; }
    ///     function __isset($n) { echo "__isset $n\n"; return true; } }
    /// $t = new T; unset($t->i);
    /// var_dump($t->i, isset($t->u));
    /// unset($t->u);
    /// try { var_dump($t->u); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// __get i
    /// int(42)
    /// bool(false)
    /// __get u
    /// Value of type string returned from T::__get() must be compatible with unset property T::$u of type int
    /// ```
    ///
    /// A reference `&__get` answers is converted in its cell, so the
    /// property it names reads the conversion too, and a write fetch
    /// binds to that cell:
    ///
    /// ```php
    /// class Test { public $prop = "42"; public int $val;
    ///     public function &__get($name) { return $this->prop; } }
    /// $test = new Test; unset($test->val);
    /// $val = &$test->val;
    /// var_dump($test->prop);
    /// $test->prop = "x";
    /// var_dump($val);
    /// ```
    /// ```expect
    /// int(42)
    /// string(1) "x"
    /// ```
    fn hold_magic_get(
        &mut self,
        class: ClassId,
        name: &[u8],
        getter: Function,
        got: Value,
    ) -> Result<Value, Thrown> {
        let Some(placed) = self
            .declared_slot(class, name)
            .filter(|p| p.typed && self.reaches(p))
        else {
            return Ok(got);
        };
        let Some(typed) = self.declared_typed_property(class, name) else {
            return Ok(got);
        };
        let strict = self.function_strict_types(getter);
        let held = got.deref();
        let cell = got.as_php_ref();
        // A cell some typed property is already a source of converts
        // nothing: it holds the type as it is, or it is refused.
        if let Some(bound) = cell.filter(|cell| cell.heap().sources() != 0) {
            let holds = crate::ops::typed_ref_holds(
                self,
                bound.heap(),
                &typed.ty,
                placed.declaring,
                name,
                strict,
            );
            return match holds {
                Ok(true) => Ok(got),
                Ok(false) => {
                    self.release(got);
                    Err(self.magic_get_refused(placed.declaring, name, &typed.ty, &held))
                }
                Err(thrown) => {
                    self.release(got);
                    Err(thrown)
                }
            };
        }
        let admitted = match crate::ops::property_type_admits(
            self,
            &held,
            &typed.ty,
            placed.declaring,
            strict,
        ) {
            Ok(admitted) => admitted,
            Err(thrown) => {
                self.release(got);
                return Err(thrown);
            }
        };
        match admitted {
            crate::ops::Admitted::As => Ok(got),
            // `i_zend_check_property_type` converts the value where it
            // stands, which for a reference `__get` answered is in the
            // cell, so every name bound to it reads the conversion.
            crate::ops::Admitted::Converted(coerced) => {
                if let Some(cell) = cell {
                    self.reference_set(cell.heap(), coerced);
                    Ok(got)
                } else {
                    self.release(got);
                    Ok(coerced)
                }
            }
            crate::ops::Admitted::Refused => {
                self.release(got);
                Err(self.magic_get_refused(placed.declaring, name, &typed.ty, &held))
            }
        }
    }

    /// `zend_magic_get_property_type_inconsistency_error`: php's
    /// `TypeError` for what `__get` answered for an unset typed property
    /// whose type holds it in no form.
    fn magic_get_refused(
        &mut self,
        declaring: ClassId,
        name: &[u8],
        ty: &php_unit::decl::Type,
        held: &Value,
    ) -> Thrown {
        let given = crate::ops::type_name(self, held);
        let class = self.class_name(declaring);
        let ty = php_unit::type_to_string(ty);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw_error(
            self,
            "TypeError",
            &format!(
                "Value of type {given} returned from {class}::__get() must be compatible with unset property {class}::${shown} of type {ty}"
            ),
        )
    }

    /// `zend_readonly_property_modification_error` (`Cannot modify
    /// readonly property C::$p`) and
    /// `zend_readonly_property_indirect_modification_error` (`Cannot
    /// indirectly modify ...`), by `operation`, naming the class that
    /// declared the property.
    ///
    /// Refused from the class's own scope as from any other, the unwritten
    /// property included where the write is indirect; `__clone` writes it
    /// once more, never indirectly:
    ///
    /// ```php
    /// class T {
    ///     public readonly int $i; public readonly array $a;
    ///     function __construct() { $this->i = 1; }
    ///     function bump() { try { $this->i++; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    ///         try { $this->i += 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    ///         try { $this->a[] = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    ///         try { unset($this->i); } catch (Error $e) { echo $e->getMessage(), "\n"; } }
    ///     function __clone() { $this->i = 5;
    ///         try { $this->i = 6; } catch (Error $e) { echo $e->getMessage(), "\n"; } }
    /// }
    /// $t = new T; $t->bump();
    /// var_dump((clone $t)->i);
    /// ```
    /// ```expect
    /// Cannot modify readonly property T::$i
    /// Cannot modify readonly property T::$i
    /// Cannot indirectly modify readonly property T::$a
    /// Cannot unset readonly property T::$i
    /// Cannot modify readonly property T::$i
    /// int(5)
    /// ```
    pub(crate) fn readonly_modification(
        &mut self,
        declaring: ClassId,
        name: &[u8],
        operation: &str,
    ) -> Thrown {
        let class = self.class_name(declaring);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Cannot {operation} readonly property {class}::${shown}"),
        )
    }

    /// php's `Error` for a typed property read before anything wrote it
    /// (`zend_throw_error` under `uninit_error`), naming the class that
    /// declared it, `prop_info->ce`.
    pub(crate) fn uninitialized_property(&mut self, declaring: ClassId, name: &[u8]) -> Thrown {
        let class = self.class_name(declaring);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Typed property {class}::${shown} must not be accessed before initialization"),
        )
    }

    /// `zend_get_property_offset`'s other wrong offset: a name no class
    /// declared that starts with a NUL byte, which is how a mangled
    /// private or protected name reads, is no property PHP code may
    /// reach, and is no dynamic one.
    pub(crate) fn bad_property_name(name: &[u8]) -> bool {
        name.first() == Some(&0)
    }

    /// [`Request::bad_property_name`] over a name `class` did not
    /// declare, which is the wrong offset `zend_get_property_offset`
    /// answers: a `__get` is asked for it whatever its guard says, and
    /// the guard standing is [`Request::wrong_offset`]'s `Error`.
    fn wrong_name(&self, class: ClassId, name: &[u8]) -> bool {
        Self::bad_property_name(name) && self.declared_slot(class, name).is_none()
    }

    /// `zend_bad_property_name`: php's `Error` for a name
    /// [`Request::bad_property_name`] refuses.
    ///
    /// ```php
    /// class A { function __unset($p) { unset($this->$p); }
    ///     function __set($p, $v) { $this->$p = $v; }
    ///     function __get($p) { return $this->$p; } }
    /// $a = new A; $p = "\0x"; $o = new stdClass;
    /// foreach ([fn() => $a->$p, fn() => $a->$p = 1, function () use ($a, $p) { unset($a->$p); },
    ///           fn() => $o->$p, fn() => $o->$p = 1, fn() => $o->$p[] = 1, fn() => $o->$p++] as $f) {
    ///     try { $f(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// var_dump(isset($o->$p), isset($a->$p), property_exists($o, $p));
    /// ```
    /// ```expect
    /// Cannot access property starting with "\0"
    /// Cannot access property starting with "\0"
    /// Cannot access property starting with "\0"
    /// Cannot access property starting with "\0"
    /// Cannot access property starting with "\0"
    /// Cannot access property starting with "\0"
    /// Cannot access property starting with "\0"
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// ```
    pub(crate) fn bad_property_name_error(&mut self) -> Thrown {
        crate::api::raising::throw_error(
            self,
            "Error",
            "Cannot access property starting with \"\\0\"",
        )
    }

    /// `zend_wrong_offset`: the placement a magic accessor's guard
    /// kept silent made again, loudly, where the guard stands and the
    /// accessor cannot answer: the `Error` for a declared property the
    /// scope may not reach, or for a name that starts with a NUL byte.
    fn wrong_offset(&mut self, class: ClassId, name: &[u8]) -> Option<Thrown> {
        if let Some(placed) = self.declared_slot(class, name) {
            return (!self.reaches(&placed))
                .then(|| self.inaccessible_property(class, placed.visibility, name));
        }
        let miss = self.unplaced(class, name, true)?;
        Some(self.unplaced_error(class, name, miss))
    }

    /// `zend_bad_property_access`: php's `Error` for a property the
    /// scope may not reach.
    pub(super) fn inaccessible_property(
        &mut self,
        class: ClassId,
        visibility: Visibility,
        name: &[u8],
    ) -> Thrown {
        let word = match visibility {
            Visibility::Public => "public",
            Visibility::Protected => "protected",
            Visibility::Private => "private",
        };
        let class = self.class_name(class);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Cannot access {word} property {class}::${shown}"),
        )
    }

    /// `zobj->ce->__get` and its three neighbours: the magic accessor
    /// `lowered` names, which php resolves once at link time and holds
    /// on the class entry as the pointer every handler tests. Each of
    /// the four is public and takes a receiver, which
    /// `zend_check_magic_method_implementation` refuses a class that
    /// declares otherwise, so the scope that reached the property never
    /// bars the call.
    fn magic_accessor(&self, class: ClassId, lowered: &[u8]) -> Option<Function> {
        self.method_named(class, lowered)
    }

    /// `zobj->ce->__set != NULL`, the test a handler makes before it
    /// places a name: whether the class serves the magic accessor
    /// `lowered` names, which is what makes a placement silent
    /// ([`Request::declared_slot_for_write`]).
    pub(crate) fn serves_accessor(&self, class: ClassId, lowered: &[u8]) -> bool {
        self.magic_accessor(class, lowered).is_some()
    }

    /// `ce->ce_flags & ZEND_ACC_USE_GUARDS`: whether `class` or a class
    /// above it declares `__get`, `__set`, `__isset`, `__unset` or
    /// `__debugInfo` (`zend_add_magic_method` sets the bit for each,
    /// and `zend_do_inheritance` passes it down), or implements an
    /// interface that hands the bit on
    /// ([`php_unit::decl::ClassFlags::use_guards`]).
    pub(crate) fn uses_guards(&self, class: ClassId) -> bool {
        const MAGIC: [&[u8]; 5] = [b"__get", b"__set", b"__isset", b"__unset", b"__debuginfo"];
        MAGIC
            .iter()
            .any(|name| self.method_named(class, name).is_some())
            || self.guards_handed_on(class, 0)
    }

    /// Whether `class`, a parent of it or an interface of either carries
    /// [`php_unit::decl::ClassFlags::use_guards`]; a chain longer than any
    /// declaration can make is cut, as [`Request::instance_of`] cuts it.
    fn guards_handed_on(&self, class: ClassId, depth: u32) -> bool {
        let Some(decl) = self.class_decl(class) else {
            return false;
        };
        if decl.flags.use_guards {
            return true;
        }
        if depth > 64 {
            return false;
        }
        decl.interfaces
            .iter()
            .chain(decl.parent.as_ref())
            .filter_map(|name| self.class_named(name.into()))
            .any(|above| self.guards_handed_on(above, depth + 1))
    }

    /// [`crate::api::values::PhpObject::is_recursive_in`].
    pub(crate) fn recursion_guarded(
        &self,
        object: PhpObject,
        walk: crate::api::values::RecursionGuard,
    ) -> bool {
        if !self.uses_guards(object.class()) {
            return object.is_recursive();
        }
        let id = object.heap().id();
        self.properties.recursion_guards.contains(&(id, walk))
    }

    /// [`crate::api::values::PhpObject::protect_in`].
    pub(crate) fn recursion_protect(
        &mut self,
        object: PhpObject,
        walk: crate::api::values::RecursionGuard,
    ) {
        if !self.uses_guards(object.class()) {
            object.protect();
            return;
        }
        let row = (object.heap().id(), walk);
        if !self.properties.recursion_guards.contains(&row) {
            self.properties.recursion_guards.push(row);
        }
    }

    /// [`crate::api::values::PhpObject::unprotect_in`].
    pub(crate) fn recursion_unprotect(
        &mut self,
        object: PhpObject,
        walk: crate::api::values::RecursionGuard,
    ) {
        if !self.uses_guards(object.class()) {
            object.unprotect();
            return;
        }
        let row = (object.heap().id(), walk);
        self.properties.recursion_guards.retain(|held| *held != row);
    }

    /// `*zend_get_property_guard(zobj, name)`: the word `name` carries
    /// on `object`, zero for a name no accessor is serving.
    pub(super) fn guard_word(&self, object: PhpObject, name: &[u8]) -> u32 {
        let id = object.heap().id();
        self.properties
            .guards
            .iter()
            .find(|((held, under), _)| *held == id && under == name)
            .map_or(0, |(_, word)| *word)
    }

    /// `(*guard) |= bit`, the row laid down by the first accessor to
    /// take it.
    pub(super) fn guard_take(&mut self, object: PhpObject, name: &[u8], bit: u32) {
        let id = object.heap().id();
        if let Some((_, word)) = self
            .properties
            .guards
            .iter_mut()
            .find(|((held, under), _)| *held == id && under == name)
        {
            *word |= bit;
            return;
        }
        self.properties.guards.push(((id, name.to_vec()), bit));
    }

    /// `(*guard) &= ~bit`, and the row dropped once the word is empty.
    /// php frees the word with the object; a row here is gone before
    /// the accessor that took it returns, so an `spl_object_id()` the
    /// store hands out again names no row of an object that has died.
    pub(super) fn guard_drop(&mut self, object: PhpObject, name: &[u8], bit: u32) {
        let id = object.heap().id();
        let at = self
            .properties
            .guards
            .iter()
            .position(|((held, under), _)| *held == id && under == name);
        let Some(at) = at else {
            return;
        };
        let Some((_, word)) = self.properties.guards.get_mut(at) else {
            return;
        };
        *word &= !bit;
        if *word == 0 {
            self.properties.guards.swap_remove(at);
        }
    }

    /// `zend_std_call_getter` and its three neighbours
    /// (`Z/Zend/zend_object_handlers.c:836`): one magic accessor served
    /// on the object with the property's name as its first argument and
    /// `arguments` after it, under `bit` in the property guard, which is
    /// taken before the call and given back after it whatever the call
    /// did. The object is one more owner for the span of the call
    /// (`GC_ADDREF`, `OBJ_RELEASE`), so an accessor that drops the last
    /// other owner does not free the object under the handler.
    fn call_accessor(
        &mut self,
        object: PhpObject,
        accessor: Function,
        bit: u32,
        name: &[u8],
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        self.guard_take(object, name, bit);
        let served = self.serve_accessor(object, accessor, name, arguments);
        self.guard_drop(object, name, bit);
        served
    }

    /// [`Request::call_accessor`] without the guard, for the one site
    /// that holds a bit across two calls: `zend_std_has_property` keeps
    /// `IN_ISSET` standing while it asks the `__get` that `empty` reads
    /// the truth of.
    fn serve_accessor(
        &mut self,
        object: PhpObject,
        accessor: Function,
        name: &[u8],
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        let name = self.declared_string(name)?;
        let mut sent = Vec::with_capacity(arguments.len().saturating_add(1));
        sent.push(Value::string(*name));
        sent.extend_from_slice(arguments);
        let held = Owner::take(self, object.as_value());
        let served = self.serve_method_made(accessor, Some(object), object.class(), &sent, &[]);
        held.give_up(self);
        name.give_up(self);
        served
    }

    /// `zend_std_read_property`, the handler `FETCH_OBJ_R` and
    /// `FETCH_OBJ_IS` call: `zend_get_property_offset` places the name,
    /// a declared property's slot where the scope may see it, php's
    /// `Error` `Cannot access private property C::$p` where it may not
    /// (`zend_bad_property_access`), and the dynamic table for a name
    /// the class did not declare. A typed slot nothing has written is
    /// php's `Error` for a property read before initialization; an
    /// untyped one that `unset` emptied reads as a dynamic name does,
    /// and a name neither holds warns `Undefined property: C::$p` and
    /// is `null`. What is read is one more owner of the value
    /// (`ZVAL_COPY_DEREF`), a reference followed.
    ///
    /// A class with a `__get` is served between the slot and the
    /// warning, under `IN_GET` in the property guard: the placement is
    /// silent where a `__get` may serve what it refuses
    /// (`zend_get_property_offset(ce, name, (type == BP_VAR_IS) ||
    /// (ce->__get != NULL), ...)`), so a property the scope may not
    /// reach is the wrong-offset marker and reaches the accessor rather
    /// than the `Error`; the uninitialised typed slot is the one arm
    /// that skips `__get`, php's `IS_PROP_UNINIT`; and a `__get` that
    /// reads the name it was called for finds the guard standing and
    /// falls through to the warning, with the `Error` raised again
    /// (`zend_wrong_offset`) where the placement was the one the scope
    /// may not reach.
    ///
    /// Under [`PropertyRead::Silent`], php's `BP_VAR_IS`, each of the
    /// three is `null` instead: `zend_get_property_offset` returns the
    /// wrong-offset marker rather than raising for a property the scope
    /// may not reach, the uninitialized typed slot is `ZVAL_NULL`, and
    /// the warning is the one line php guards with `type != BP_VAR_IS`.
    /// A class with a `__isset` serves it there before the `__get`, and
    /// a falsy answer is the `null` with no `__get` called at all.
    ///
    /// ```php
    /// class M { public function __get($k) { echo "get $k\n"; return 7; } }
    /// $m = new M;
    /// var_dump($m->z);
    /// class P { private $secret = 5; public function __get($k) { echo "get $k\n"; return 9; } }
    /// $p = new P;
    /// var_dump($p->secret);
    /// class T { public int $t; public function __get($k) { echo "get $k\n"; return 1; } }
    /// $t = new T;
    /// try { var_dump($t->t); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// get z
    /// int(7)
    /// get secret
    /// int(9)
    /// Error: Typed property T::$t must not be accessed before initialization
    /// ```
    ///
    /// The guard: a `__get` that reads the name it was called for finds
    /// it standing and warns where the recursion would have been.
    ///
    /// ```php
    /// class R { public function __get($k) { echo "in get $k\n"; return $this->a; } }
    /// $r = new R;
    /// var_dump($r->a);
    /// ```
    /// ```expect
    /// in get a
    ///
    /// Warning: Undefined property: R::$a in %s on line 2
    /// NULL
    /// ```
    ///
    /// A lazy object is initialised where the read finds nothing
    /// (`zend_lazy_object_init` at `uninit_error`): a declared slot that
    /// waits skips `__get` and initialises the object, while a name no
    /// class declared is the accessor's first.
    ///
    /// ```php
    /// class C { public $a = 1; public function __get($n) { echo "get $n\n"; return 0; } }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function ($o) { echo "init\n"; });
    /// var_dump($o->dyn);
    /// var_dump($r->isUninitializedLazyObject($o));
    /// var_dump($o->a);
    /// ```
    /// ```expect
    /// get dyn
    /// int(0)
    /// bool(true)
    /// init
    /// int(1)
    /// ```
    pub(crate) fn read_property(
        &mut self,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        let silent = read == PropertyRead::Silent;
        // What nothing holds is `null`, quietly, under `BP_VAR_IS` and
        // `BP_VAR_UNSET`; only the first asks `__isset`.
        let quiet = matches!(read, PropertyRead::Silent | PropertyRead::Unset);
        let o = object.heap();
        let class = object.class();
        if let Some(read) = self.read_hooked_arm(object, name, read) {
            return adopted(read);
        }
        // `ZEND_WRONG_PROPERTY_OFFSET`: the placement found a property
        // the scope may not reach and said so quietly, because a
        // `__get` may serve it. php tests one pointer for that; the
        // class is asked for the accessor only where the answer is
        // read, so a slot that holds a value is placed and returned
        // without a lookup.
        let mut wrong = false;
        if let Some(placed) = self.declared_slot(class, name) {
            if self.reaches(&placed) {
                let z = o.slot(placed.slot);
                if !z.is_undef() {
                    return Ok(Owner::take(self, Value::of_slot(z).deref()));
                }
                // `IS_PROP_UNINIT | IS_PROP_LAZY`: a slot that waits for
                // the initializer skips `__get` too, and the object is
                // initialised and read instead (`uninit_error`).
                if self.lazy_slot(o, placed.slot) {
                    let instance = self.lazy_init(object)?;
                    return self.read_property(instance, name, read);
                }
                // `IS_PROP_UNINIT`: a typed slot nothing has written
                // skips `__get` and is the uninitialised-access error; one
                // an `unset` emptied is the magic methods' where the class
                // has one that answers, and the same error where not
                // (`uninit_error` with `prop_info` set).
                let magic = self.serves_accessor(class, b"__get")
                    || (silent && self.serves_accessor(class, b"__isset"));
                if placed.typed && (o.slot_uninit(placed.slot) || !magic) {
                    if quiet {
                        return Ok(Owner::null());
                    }
                    return Err(self.uninitialized_property(placed.declaring, name));
                }
            } else {
                if !silent && !self.serves_accessor(class, b"__get") {
                    return Err(self.inaccessible_property(class, placed.visibility, name));
                }
                wrong = true;
            }
        } else if let Some(miss) =
            self.unplaced(class, name, silent || self.serves_accessor(class, b"__get"))
        {
            if !silent && !self.serves_accessor(class, b"__get") {
                return Err(self.unplaced_error(class, name, miss));
            }
            wrong = true;
        } else if let Some(value) = Self::dynamic_prop(o, name) {
            return Ok(Owner::take(self, value.deref()));
        }
        let getter = self.magic_accessor(class, b"__get");
        let issetter = silent
            .then(|| self.magic_accessor(class, b"__isset"))
            .flatten();
        let guard_bit = if issetter.is_some() { IN_ISSET } else { IN_GET };
        // GH-21478: a proxy that forwards, read from inside the
        // instance's own accessor for this name, reads the instance
        // directly rather than serving the accessor twice.
        if let Some(instance) = self.lazy_instance(o)
            && self.guard_word(instance, name) & guard_bit != 0
        {
            return self.read_property(instance, name, read);
        }
        // `/* magic isset */`: under `BP_VAR_IS` a class with a
        // `__isset` is asked first, and the `__get` after it only where
        // it said yes, the object held across both (`GC_ADDREF(zobj)`
        // before the `__isset`, `OBJ_RELEASE` after the `__get`), so
        // an `__isset` that gives up every other owner does not free
        // the object the `__get` is served on.
        if let Some(issetter) = issetter {
            let held = Owner::take(self, object.as_value());
            let served = self.isset_then_get(object, name, read, issetter, getter);
            held.give_up(self);
            if let Some(served) = served? {
                return Ok(Owner::adopt(served));
            }
        } else if let Some(getter) = getter {
            if self.guard_word(object, name) & IN_GET == 0 {
                let got = self.call_accessor(object, getter, IN_GET, name, &[])?;
                self.overloaded_write_notice(class, name, read, &got);
                let held = self.hold_magic_get(class, name, getter, got);
                return adopted(held);
            }
            // `zend_wrong_offset`: the guard stands and the placement
            // was a wrong offset, so it is made again, loudly, and the
            // `Error` it raises is the answer.
            if wrong && let Some(thrown) = self.wrong_offset(class, name) {
                return Err(thrown);
            }
        }
        // `uninit_error` over a lazy object: a name nothing holds
        // initialises it and is read on what that answers, under the
        // guard the accessor above stood under where one did.
        if o.laziness() != php_heap::Laziness::Eager {
            let instance = self.lazy_init(object)?;
            let guarded = getter.is_some() || issetter.is_some();
            if guarded && self.guard_word(instance, name) & guard_bit == 0 {
                self.guard_take(instance, name, guard_bit);
                let read = self.read_property(instance, name, read);
                self.guard_drop(instance, name, guard_bit);
                return read;
            }
            return self.read_property(instance, name, read);
        }
        if !quiet {
            let class = self.class_name(class);
            let shown = String::from_utf8_lossy(name);
            crate::api::raising::warning(self, &format!("Undefined property: {class}::${shown}"));
        }
        Ok(Owner::null())
    }

    /// `zend_std_write_property`'s `found` arm over a slot that holds a
    /// value: an initialised readonly property is modified from no scope,
    /// save once more under `IS_PROP_REINITABLE`, and a set visibility
    /// the scope lacks refuses the write too; `None` lets it through.
    pub(super) fn found_refusal(
        &mut self,
        o: Obj,
        p: &DeclaredSlot,
        name: &[u8],
    ) -> Option<Thrown> {
        if p.readonly && !o.slot_bit(p.slot, php_heap::PropBit::Reinitable) {
            return Some(self.readonly_modification(p.declaring, name, "modify"));
        }
        self.set_refusal(p, name, "modify")
    }

    /// The set visibility of the declared slot `placed` where the scope
    /// lacks it (`zend_asymmetric_property_has_set_access`), none where
    /// it has it or the property has none.
    fn set_barred(&self, placed: &DeclaredSlot) -> Option<Visibility> {
        let set = placed.set_visibility?;
        (!self.has_set_access(placed.declaring, placed.root, set)).then_some(set)
    }

    /// [`Request::set_inaccessible_property`]'s `Error` for `operation`
    /// on the declared slot `placed`, where its set visibility refuses the
    /// scope ([`Request::set_barred`]); none where it lets it through.
    fn set_refusal(
        &mut self,
        placed: &DeclaredSlot,
        name: &[u8],
        operation: &str,
    ) -> Option<Thrown> {
        let set = self.set_barred(placed)?;
        let (declaring, readonly) = (placed.declaring, placed.readonly);
        Some(self.set_inaccessible_property(declaring, set, readonly, name, operation))
    }

    /// Whether `zend_std_get_property_ptr_ptr`
    /// (`Z/Zend/zend_object_handlers.c:1485`) hands out no place for the
    /// declared property `name` the scope reaches, so that a compound
    /// assignment is `read_property`, the operation and `write_property`:
    /// a readonly property, and one whose set visibility the scope lacks.
    /// What the operation throws then comes before what the write
    /// refuses.
    ///
    /// ```php
    /// class Test { public readonly object $prop; public private(set) int $n = 1;
    ///     function __construct() { $this->prop = new stdClass; } }
    /// $t = new Test;
    /// try { $t->prop += 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $t->prop .= "x"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $t->n += 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Unsupported operand types: stdClass + int
    /// Object of class stdClass could not be converted to string
    /// Cannot modify private(set) property Test::$n from global scope
    /// ```
    pub(crate) fn place_withheld(&self, object: PhpObject, name: &[u8]) -> bool {
        let Some(placed) = self.declared_slot(object.class(), name) else {
            return false;
        };
        if !self.may_access(placed.declaring, placed.visibility) {
            return false;
        }
        placed.readonly || self.set_barred(&placed).is_some()
    }

    /// `zend_get_property_offset` at a write site, what `FETCH_OBJ_W`
    /// and `ASSIGN_OBJ` place the name by: the declared slot `name` sits
    /// in and whether it is typed, where the executed scope may reach
    /// it; php's `Error` `Cannot access private property C::$p` where
    /// it may not (`zend_bad_property_access`); and none for a name the
    /// class did not declare, which is a dynamic property. A property
    /// with a set visibility is held to that one as well
    /// ([`Request::has_set_access`]), which is the second check
    /// `zend_std_write_property` and `zend_std_get_property_ptr_ptr`
    /// make; `operation` is the word the site's error carries.
    ///
    /// `silent` is php's own argument to `zend_get_property_offset`: a
    /// site whose class serves the accessor that may take the write
    /// places the name quietly, so a property the scope may not reach
    /// is `None`, the wrong-offset marker, and the handler goes on to
    /// `write_property` or to the overloaded pair, where the `Error` is
    /// raised again if no accessor takes it.
    ///
    /// ```php
    /// class Foo { public private(set) int $bar = 1; public protected(set) int $baz = 2; }
    /// $f = new Foo;
    /// try { $f->bar = 3; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $f->baz = 4; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $f->bar->x = 5; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { unset($f->bar); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($f->bar, $f->baz);
    /// ```
    /// ```expect
    /// Cannot modify private(set) property Foo::$bar from global scope
    /// Cannot modify protected(set) property Foo::$baz from global scope
    /// Cannot indirectly modify private(set) property Foo::$bar from global scope
    /// Cannot unset private(set) property Foo::$bar from global scope
    /// int(1)
    /// int(2)
    /// ```
    ///
    /// Uncaught, the write from global scope is php's `Error`:
    ///
    /// ```php
    /// class Foo { public private(set) int $bar = 1; }
    /// $f = new Foo;
    /// $f->bar = 2;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Cannot modify private(set) property Foo::$bar from global scope in %s:4
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 4
    /// ```
    ///
    /// A property the class declares with a set visibility and no
    /// default is uninitialised like any other typed one:
    ///
    /// ```php
    /// class Foo { public private(set) int $bar; }
    /// $f = new Foo;
    /// var_dump($f->bar);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Typed property Foo::$bar must not be accessed before initialization in %s:4
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 4
    /// ```
    pub(crate) fn declared_slot_for_write(
        &mut self,
        object: PhpObject,
        name: &[u8],
        operation: &str,
        silent: bool,
    ) -> Result<Option<(u32, bool)>, Thrown> {
        let class = object.class();
        let Some(placed) = self.declared_slot(class, name) else {
            if let Some(miss) = self.unplaced(class, name, silent)
                && !silent
            {
                return Err(self.unplaced_error(class, name, miss));
            }
            return Ok(None);
        };
        if !self.reaches(&placed) {
            if silent {
                return Ok(None);
            }
            return Err(self.inaccessible_property(class, placed.visibility, name));
        }
        // A slot an `unset` emptied, on a class with a `__set`, is the
        // setter's: `zend_std_write_property` reaches the magic method
        // before the set visibility, which `write_std_property` holds the
        // write to after it.
        let o = object.heap();
        if o.slot(placed.slot).is_undef()
            && !o.slot_uninit(placed.slot)
            && !self.lazy_slot(o, placed.slot)
            && self.serves_accessor(class, b"__set")
        {
            return Ok(None);
        }
        // `zend_readonly_property_modification_error` and its indirect
        // twin, ahead of the set visibility: an initialised readonly
        // property is modified from no scope, save once more under
        // `IS_PROP_REINITABLE` inside `__clone`, and an unwritten one is
        // no place for an indirect modification either, which would
        // initialise it by a write the property never sees.
        let initialized = !o.slot(placed.slot).is_undef();
        // `IS_PROP_REINITABLE` lets `__clone` write the property again,
        // never modify it indirectly (`zend_std_read_property` under
        // `BP_VAR_W` does not read the bit).
        let indirect = operation == "indirectly modify";
        if placed.readonly
            && (indirect
                || (initialized && !o.slot_bit(placed.slot, php_heap::PropBit::Reinitable)))
        {
            return Err(self.readonly_modification(placed.declaring, name, operation));
        }
        // The set visibility after the readonly arm, which a readonly
        // property `IS_PROP_REINITABLE` has passed: `__clone` and a
        // `clone with` write it once more only from a scope that may set
        // it (`zend_std_write_property`).
        if let Some(thrown) = self.set_refusal(&placed, name, operation) {
            return Err(thrown);
        }
        Ok(Some((placed.slot, placed.typed)))
    }

    /// `if (UNEXPECTED(Z_TYPE_P(retval) == IS_UNDEF))` in
    /// `zend_std_get_property_ptr_ptr` under `BP_VAR_RW`, ahead of any
    /// placement: a declared slot the scope reaches that nothing holds,
    /// where no `__get` answers for it or it is `IS_PROP_UNINIT`, raises
    /// the read's own diagnostic, the typed property's `Error` naming the
    /// class that declared it or the warning, an untyped one laid to
    /// `null` before the warning as php lays it. Anything else is the
    /// placement's.
    pub(crate) fn read_before_write(
        &mut self,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        let class = object.class();
        let o = object.heap();
        let Some(placed) = self.declared_slot(class, name) else {
            return Ok(());
        };
        if !self.reaches(&placed)
            || !o.slot(placed.slot).is_undef()
            || self.lazy_slot(o, placed.slot)
        {
            return Ok(());
        }
        let magic =
            self.serves_accessor(class, b"__get") && self.guard_word(object, name) & IN_GET == 0;
        if magic && !o.slot_uninit(placed.slot) {
            return Ok(());
        }
        if placed.typed {
            return Err(self.uninitialized_property(placed.declaring, name));
        }
        // `ZVAL_NULL(retval)` before the warning, so what an error handler
        // writes into the slot is what the fetch then reads.
        self.write_declared_slot(o, placed.slot, Value::null())?;
        let class = self.class_name(class);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::warning(self, &format!("Undefined property: {class}::${shown}"));
        Ok(())
    }

    /// Whether a write fetch over `name` reads the property rather than
    /// placing it: a declared slot the scope reaches whose readonly or
    /// set visibility bars this scope from writing it, holding an object.
    /// php's `zend_std_get_property_ptr_ptr` hands such a slot to
    /// `read_property`, which under `BP_VAR_W` returns the object as a
    /// copy (`zend_std_read_property`), so a write through the object
    /// changes the object and never the property.
    ///
    /// Under an `unset` fetch the same slot holding nothing is read too,
    /// as `null` and quietly: there is nothing to modify.
    pub(crate) fn write_fetch_reads_object(
        &self,
        object: PhpObject,
        name: &[u8],
        unset: bool,
    ) -> bool {
        let Some(placed) = self.declared_slot(object.class(), name) else {
            return false;
        };
        let barred = placed.readonly || self.set_barred(&placed).is_some();
        let held = object.heap().slot(placed.slot);
        barred
            && self.reaches(&placed)
            && (Value::of_slot(held).deref().as_object().is_some() || (unset && held.is_undef()))
    }

    /// The `zend_property_info` beside a typed slot, what
    /// `zend_assign_to_typed_prop` is handed: the declared type, the
    /// class that declared it, and `ZEND_ACC_READONLY`. Read only where
    /// [`Request::declared_slot_for_write`] said the slot is typed, so
    /// an untyped write pays nothing for it; a property php lets carry
    /// `readonly` is typed by php's own refusal, so no untyped write
    /// misses the bit.
    pub(crate) fn declared_typed_property(
        &self,
        class: ClassId,
        name: &[u8],
    ) -> Option<TypedProperty> {
        let (_, declaring, p) = self.placed_row(class, name)?;
        p.ty.clone().map(|ty| TypedProperty {
            declaring,
            ty,
            readonly: p.is_readonly,
        })
    }

    /// [`Request::declared_typed_property`] for a `static` property: the
    /// `zend_property_info` `zend_fetch_static_property_address` hands on,
    /// its type and the class that declared it, where the property is
    /// typed.
    pub(crate) fn declared_typed_static(
        &self,
        class: ClassId,
        name: &[u8],
    ) -> Option<TypedProperty> {
        let (declaring, p) = self.property_info_of(class, name)?;
        if p.is_instance {
            return None;
        }
        p.ty.clone().map(|ty| TypedProperty {
            declaring,
            ty,
            readonly: p.is_readonly,
        })
    }

    /// `zend_verify_property_type` over a value about to land in a typed
    /// property, the step `zend_assign_to_typed_prop`,
    /// `zend_binary_assign_op_typed_prop`, `zend_incdec_typed_prop` and
    /// `zend_std_write_property` share: the value as given where the
    /// declared type accepts it, the weak rule's conversion in its place
    /// where `strict` is off and the rule makes one, and php's
    /// `TypeError` otherwise. `value` is given up on every arm that does
    /// not land it.
    pub(crate) fn hold_to_property_type(
        &mut self,
        typed: &TypedProperty,
        name: &[u8],
        value: Value,
        strict: bool,
    ) -> Result<Value, Thrown> {
        let verified = crate::ops::verify_property_type(
            self,
            &value,
            &typed.ty,
            typed.declaring,
            name,
            strict,
        );
        crate::ops::landed(self, value, verified)
    }

    /// `zend_std_write_property`'s `typed_property` arm around
    /// [`Request::hold_to_property_type`]: the object held one owner more
    /// while the value is verified (`GC_ADDREF(zobj)`), since a
    /// `__toString` the weak rule serves may give its last owner up, and
    /// where that hold was the last one, php's `Error` and the object
    /// destroyed after it, the value given up (`GC_DELREF(zobj) == 0`,
    /// `Z/Zend/zend_object_handlers.c:1113`).
    ///
    /// ```php
    /// class A { public string $prop; }
    /// class B { public function __toString() { global $a; $a = null; return "a"; } }
    /// $a = new A();
    /// try { $a->prop = new B(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $a = new A(); $a->prop = '';
    /// try { $a->prop = new B(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($a);
    /// ```
    /// ```expect
    /// Object was released while assigning to property A::$prop
    /// Object was released while assigning to property A::$prop
    /// NULL
    /// ```
    pub(crate) fn hold_to_property_type_of(
        &mut self,
        object: PhpObject,
        typed: &TypedProperty,
        name: &[u8],
        value: Value,
        strict: bool,
    ) -> Result<Value, Thrown> {
        let held = Owner::take(self, object.as_value());
        let verified = self.hold_to_property_type(typed, name, value, strict);
        let Ok(landing) = verified else {
            held.let_go(self);
            return verified;
        };
        if object.as_value().refcount() > 1 {
            held.let_go(self);
            return Ok(landing);
        }
        let class = self.class_name(typed.declaring);
        let shown = String::from_utf8_lossy(name).into_owned();
        let error = crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Object was released while assigning to property {class}::${shown}"),
        );
        held.let_go(self);
        self.release(landing);
        Err(error)
    }

    /// `zend_assign_to_variable` over a declared slot, `ASSIGN_OBJ`'s
    /// fast path over a slot that holds a value (`fast_assign_obj`):
    /// `value` is the slot's from here and what it held is released
    /// last, after the write, which is where a destructor is served; a
    /// slot holding a reference is written through the cell
    /// (`Z_ISREF_P(variable_ptr)`), so every holder of the cell reads
    /// the new value. A cell some typed property is a source of is
    /// `zend_assign_to_typed_ref`, held to each of their types under
    /// `strict` ([`Request::assign_to_reference`]), whatever the slot's
    /// own property declares. What landed comes back, a view: the
    /// coercion where the cell's types made one. Beside it comes what the
    /// slot held, one owner, for the caller to release after it has
    /// copied what landed into `result`, as php's `garbage` is given up
    /// last: a destructor that writes the property again does not change
    /// the value the assignment answers.
    ///
    /// ```php
    /// class C { public int $foo = 1; public $_; }
    /// $a = new C; $a->_ = &$a->foo;
    /// try { $a->_ = []; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($a->_ = "5", $a->foo);
    /// ```
    /// ```expect
    /// Cannot assign array to reference held by property C::$foo of type int
    /// int(5)
    /// int(5)
    /// ```
    ///
    /// What the slot held is given up after the assignment's value is
    /// copied, so a destructor it serves that writes the property again
    /// changes the property and not the value:
    ///
    /// ```php
    /// class Box { public ?Test $value; public static ?Test $s; }
    /// class Test { function __destruct() { global $box; $box->value = null; Box::$s = null; } }
    /// $box = new Box; $box->value = new Test; Box::$s = new Test;
    /// var_dump($box->value = new Test, Box::$s = new Test, $box->value, Box::$s);
    /// ```
    /// ```expect
    /// object(Test)#4 (0) {
    /// }
    /// object(Test)#2 (0) {
    /// }
    /// NULL
    /// object(Test)#2 (0) {
    /// }
    /// ```
    pub(crate) fn assign_declared_slot(
        &mut self,
        o: Obj,
        i: u32,
        value: Value,
        strict: bool,
    ) -> Result<(Value, Value), Thrown> {
        if let Some(r) = o.slot(i).as_reference() {
            let cell = PhpRef::of(r);
            let garbage = cell.get();
            self.addref(&garbage);
            if let Err(thrown) = self.assign_to_reference(r, value, strict) {
                self.release(garbage);
                return Err(thrown);
            }
            return Ok((cell.get(), garbage));
        }
        let garbage = Value::of_slot(o.slot(i));
        self.addref(&garbage);
        Heap::object_set_slot(&mut Hooks(self), o, i, value.into_zval());
        Ok((value, garbage))
    }

    /// `OBJ_PROP_NUM(obj, i)` written with no name in hand, what a
    /// native that laid the object down does (`add_token`): the slot is
    /// `value`'s from here and what it held is released last, as
    /// [`Heap::object_set_slot`] does. An index past the declared count
    /// is the internal-error fatal naming the class and the index, the
    /// value released, since no slot is there to take it.
    pub(crate) fn write_declared_slot(
        &mut self,
        o: Obj,
        i: u32,
        value: Value,
    ) -> Result<(), Thrown> {
        if i >= o.slots() {
            self.release(value);
            let class = self.class_name(ClassId(o.class()));
            let slots = o.slots();
            return self.fatal_of(Err(Fatal::error(format!(
                "InternalError: slot {i} written on an object of class {class}, which declares {slots}"
            ))));
        }
        Heap::object_set_slot(&mut Hooks(self), o, i, value.into_zval());
        Ok(())
    }

    /// `zend_std_write_property`, the `Std` slot: the placement, then
    /// the class's `__set` for a name it found nothing at, and
    /// otherwise `write_std_property`
    /// ([`Request::write_std_property`]).
    ///
    /// php places the name with `zend_get_property_offset(ce, name,
    /// ce->__set != NULL, ...)` and reaches the setter where the
    /// placement found no value to write over: a name no class declared
    /// that the dynamic table does not hold, a declared untyped slot an
    /// `unset` emptied, and a declared slot the scope may not reach,
    /// which is the wrong-offset marker a `__set` makes quiet. A
    /// declared typed slot nothing has written is the one arm that
    /// bypasses `__set`, and so is every slot that holds a value. The
    /// setter is served under `IN_SET` in the property guard, so a
    /// `__set` writing the name it was called for lays the property
    /// down instead of reaching itself; where the placement was the one
    /// the scope may not reach, that second pass is the `Error`
    /// (`zend_wrong_offset`) rather than a write.
    ///
    /// `value` is given up here whatever arm runs, as php's caller
    /// gives it up, and so is `name`. An initialised readonly slot is
    /// php's `Error`, from the declaring scope too, as Reflection writes:
    ///
    /// ```php
    /// enum Foo: int { case Bar = 0; }
    /// try { (new ReflectionProperty(Foo::class, 'value'))->setValue(Foo::Bar, 1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump(Foo::Bar->value);
    /// ```
    /// ```expect
    /// Cannot modify readonly property Foo::$value
    /// int(0)
    /// ```
    ///
    /// ```php
    /// class M { public $kept = 1; public function __set($k, $v) { echo "set $k=$v\n"; } }
    /// $m = new M;
    /// $m->y = 3;
    /// $m->kept = 2;
    /// var_dump($m);
    /// class P { private $secret = 5; public function __set($k, $v) { echo "set $k=$v\n"; } }
    /// $p = new P;
    /// $p->secret = 8;
    /// var_dump($p);
    /// ```
    /// ```expect
    /// set y=3
    /// object(M)#1 (1) {
    ///   ["kept"]=>
    ///   int(2)
    /// }
    /// set secret=8
    /// object(P)#2 (1) {
    ///   ["secret":"P":private]=>
    ///   int(5)
    /// }
    /// ```
    pub(crate) fn write_property(
        &mut self,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let class = object.class();
        // `if (Z_TYPE_P(variable_ptr) != IS_UNDEF) goto found` and
        // `if (Z_PROP_FLAG_P(variable_ptr) & IS_PROP_UNINIT) goto
        // write_std_property`: a declared slot the scope may reach that
        // holds a value, and a typed one nothing has written, are both
        // the setter's business to stay out of, so neither asks the
        // class for one.
        let o = object.heap();
        let lazy = o.laziness() != php_heap::Laziness::Eager;
        if let Some(hooked) = self.hooked(class, name.as_bytes()) {
            if self.hooked_accessible(&hooked) {
                return self.write_hooked(object, name, value, hooked);
            }
            if !self.serves_accessor(class, b"__set") {
                let thrown = self.inaccessible_property(class, hooked.visibility, name.as_bytes());
                value.give_up(self);
                name.give_up(self);
                return Err(thrown);
            }
        }
        let placed = self.declared_slot(class, name.as_bytes());
        if let Some(p) = placed.as_ref()
            && self.reaches(p)
        {
            if !o.slot(p.slot).is_undef() {
                if let Some(thrown) = self.found_refusal(o, p, name.as_bytes()) {
                    value.give_up(self);
                    name.give_up(self);
                    return Err(thrown);
                }
                return self.write_std_property(object, name, value);
            }
            // `IS_PROP_LAZY`: a slot that waits is the initializer's
            // first, then the write's on what it answers.
            if self.lazy_slot(o, p.slot) {
                return self.lazy_write(object, name, value, false);
            }
            // `IS_PROP_UNINIT`: a slot nothing has written passes `__set`
            // by; one an `unset` emptied is the setter's.
            if o.slot_uninit(p.slot) {
                return self.write_std_property(object, name, value);
            }
        }
        // `zend_get_property_offset` refusing a slot the scope may not
        // reach, silent only for a class with a `__set` to serve it.
        if let Some(p) = placed.as_ref()
            && !self.reaches(p)
            && !self.serves_accessor(class, b"__set")
        {
            let thrown = self.inaccessible_property(class, p.visibility, name.as_bytes());
            value.give_up(self);
            name.give_up(self);
            return Err(thrown);
        }
        let bad_name = placed.is_none() && Self::bad_property_name(name.as_bytes());
        if bad_name && !self.serves_accessor(class, b"__set") {
            let thrown = self.bad_property_name_error();
            value.give_up(self);
            name.give_up(self);
            return Err(thrown);
        }
        let held = placed.is_none()
            && !bad_name
            && o.properties()
                .is_some_and(|table| table.contains(php_heap::Key::Str(name.heap())));
        if held {
            return self.write_std_property(object, name, value);
        }
        // GH-21478: a proxy that forwards, written from inside the
        // instance's own `__set` for this name, writes the instance.
        if let Some(instance) = self.lazy_instance(o)
            && self.guard_word(instance, name.as_bytes()) & IN_SET != 0
        {
            return self.write_property(instance, name, value);
        }
        let Some(setter) = self.magic_accessor(class, b"__set") else {
            if lazy {
                return self.lazy_write(object, name, value, false);
            }
            return self.write_std_property(object, name, value);
        };
        let bytes = name.as_bytes().to_vec();
        let wrong = bad_name || placed.as_ref().is_some_and(|p| !self.reaches(p));
        if self.guard_word(object, &bytes) & IN_SET == 0 {
            let served = self.call_accessor(object, setter, IN_SET, &bytes, &[*value]);
            value.give_up(self);
            name.give_up(self);
            let answered = served?;
            self.release(answered);
            return Ok(());
        }
        // `zend_wrong_offset`: the guard stands, so the placement is
        // made again and the `Error` it raises is the answer.
        if wrong && let Some(thrown) = self.wrong_offset(class, &bytes) {
            value.give_up(self);
            name.give_up(self);
            return Err(thrown);
        }
        if lazy {
            return self.lazy_write(object, name, value, true);
        }
        // `write_std_property`'s set-visibility arm, which the write a
        // `__set` makes on its own property, under the guard, meets.
        if let Some(placed) = self.declared_slot(class, &bytes)
            && !placed.readonly
            && let Some(thrown) = self.set_refusal(&placed, &bytes, "modify")
        {
            value.give_up(self);
            name.give_up(self);
            return Err(thrown);
        }
        self.write_std_property(object, name, value)
    }

    /// `forward_write_to_lazy_object`
    /// (`Z/Zend/zend_object_handlers.c:1029`): the object initialised,
    /// and the write made on what that answers, under `IN_SET` there
    /// where the `__set` guard stood on the lazy object. `value` and
    /// `name` are given up whatever arm runs.
    pub(super) fn lazy_write(
        &mut self,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
        guarded: bool,
    ) -> Result<(), Thrown> {
        let instance = match self.lazy_init(object) {
            Ok(instance) => instance,
            Err(thrown) => {
                value.give_up(self);
                name.give_up(self);
                return Err(thrown);
            }
        };
        let bytes = name.as_bytes().to_vec();
        if guarded && self.guard_word(instance, &bytes) & IN_SET == 0 {
            self.guard_take(instance, &bytes, IN_SET);
            let written = self.write_property(instance, name, value);
            self.guard_drop(instance, &bytes, IN_SET);
            return written;
        }
        self.write_property(instance, name, value)
    }

    /// `write_std_property`, `zend_std_write_property`'s label and what
    /// a class with no `__set` is: into the declared
    /// slot, what it held released last, or into the dynamic table,
    /// materialised on the first write and separated when a cast shares
    /// it. The name is given up once the table has taken its own owner
    /// of it. A name the table does not hold yet is a creation and goes
    /// through [`Request::dynamic_property_creation`] first, php's two
    /// arms at `write_std_property`'s `else`; the value is given up with
    /// the `Error`, as `FREE_OP_DATA` gives it up.
    ///
    /// A typed slot is `zend_verify_property_type` under
    /// `property_uses_strict_types()` ([`Request::property_strict_types`])
    /// first: the value the declared type accepts, the weak rule's
    /// conversion where the frame the write stands in is not strict,
    /// which a native's own frame never is, and php's `TypeError` with
    /// the value given up where neither holds. The value then lands as
    /// `zend_assign_to_variable_ex` lands it, through the cell where the
    /// slot holds a reference.
    ///
    /// ```php
    /// class Foo { public int $id; public ?float $f = null; }
    /// $db = new PDO('sqlite::memory:');
    /// $db->setAttribute(PDO::ATTR_STRINGIFY_FETCHES, true);
    /// $st = $db->query("SELECT '1' AS id, '2.5' AS f");
    /// var_dump($st->fetchObject(Foo::class));
    /// $st = $db->query("SELECT 'x' AS id");
    /// try { $st->fetchObject(Foo::class); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(Foo)#3 (2) {
    ///   ["id"]=>
    ///   int(1)
    ///   ["f"]=>
    ///   float(2.5)
    /// }
    /// Cannot assign string to property Foo::$id of type int
    /// ```
    ///
    /// The weak rule holds while the native writes, though the file
    /// that called it declares `strict_types`, since php's frame there
    /// is the native's own; a reference the slot holds is written
    /// through.
    ///
    /// ```php
    /// declare(strict_types=1);
    /// class Foo { public int $id = 0; }
    /// $db = new PDO('sqlite::memory:');
    /// $st = $db->query("SELECT '7' AS id");
    /// $st->setFetchMode(PDO::FETCH_INTO, $o = new Foo);
    /// $r = &$o->id;
    /// $st->fetch();
    /// var_dump($o->id, $r);
    /// ```
    /// ```expect
    /// int(7)
    /// int(7)
    /// ```
    ///
    /// A dynamic property that holds a reference a typed property holds
    /// too is written through `zend_assign_to_typed_ref`, an undefined
    /// value as the `null` its warning left.
    ///
    /// ```php
    /// class T { public string $x = ""; public int $n = 1; }
    /// $t = new T; $o = new stdClass;
    /// $o->p =& $t->x;
    /// try { $o->p = $v; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// $o->q =& $t->n;
    /// try { $o->q = "abc"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// $o->q = "5";
    /// var_dump($t->x, $t->n);
    /// ```
    /// ```expect
    ///
    /// Warning: Undefined variable $v in %s on line 5
    /// Cannot assign null to reference held by property T::$x of type string
    /// Cannot assign string to reference held by property T::$n of type int
    /// string(0) ""
    /// int(5)
    /// ```
    pub(super) fn write_std_property(
        &mut self,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let o = object.heap();
        if let Some(placed) = self.declared_slot(object.class(), name.as_bytes()) {
            let value = if placed.typed {
                let Some(typed) = self.declared_typed_property(object.class(), name.as_bytes())
                else {
                    value.give_up(self);
                    name.give_up(self);
                    return self.fatal_of(Err(Fatal::error(
                        "InternalError: write_property placed a typed property whose declared type the class does not carry",
                    )));
                };
                let strict = self.property_strict_types();
                let value = value.hand_on();
                match self.hold_to_property_type_of(object, &typed, name.as_bytes(), value, strict)
                {
                    Ok(value) => value,
                    Err(thrown) => {
                        name.give_up(self);
                        return Err(thrown);
                    }
                }
            } else {
                value.hand_on()
            };
            match self.assign_declared_slot(o, placed.slot, value, self.property_strict_types()) {
                Ok((_, garbage)) => self.release(garbage),
                Err(thrown) => {
                    name.give_up(self);
                    return Err(thrown);
                }
            }
        } else {
            let key = php_heap::Key::Str(name.heap());
            // `zend_array_dup` over a table a clone or a cast shares,
            // before the entry is looked for, so a reference the other
            // holder's copy owns alone is its value here and not a cell
            // both write through.
            if o.properties().is_some_and(|table| table.refcount() > 1) {
                match self.separated_properties(o) {
                    Ok(_) => {}
                    Err(thrown) => {
                        value.give_up(self);
                        name.give_up(self);
                        return Err(thrown);
                    }
                }
            }
            let held = o.properties().is_some_and(|table| table.contains(key));
            // `zend_assign_to_variable` over the entry found: one that
            // holds a reference, what `$o->p = &$x` bound, is written
            // through, so every name of the cell reads the value, and
            // held to the types of the properties that hold the cell
            // (`zend_assign_to_typed_ref`) under the writing file's rule.
            let bound = o
                .properties()
                .and_then(|table| table.get(key))
                .and_then(|found| Value::of_slot(found).as_php_ref());
            if let Some(cell) = bound {
                name.give_up(self);
                let strict = self.property_strict_types();
                return self.assign_to_reference(
                    cell.heap(),
                    value.hand_on().undef_as_null(),
                    strict,
                );
            }
            if !held && let Err(thrown) = self.dynamic_property_creation(object, name.as_bytes()) {
                value.give_up(self);
                name.give_up(self);
                return Err(thrown);
            }
            let table = match self.separated_properties(o) {
                Ok(table) => table,
                Err(thrown) => {
                    value.give_up(self);
                    name.give_up(self);
                    return Err(thrown);
                }
            };
            if let Err(thrown) = self.array_update(table, key, value.hand_on()) {
                name.give_up(self);
                return Err(thrown);
            }
        }
        name.give_up(self);
        Ok(())
    }

    /// `process_nested_object_data`'s write of one property
    /// (`Z/ext/standard/var_unserializer.re:554`): `key` as a serialized
    /// object carries it, the name plain for a public property, `\0*\0`
    /// before a protected one's and `\0Class\0` before a private one's.
    /// A key that names a declared property is written into its slot
    /// directly, visibility, `readonly` and `__set` not consulted, as php
    /// writes `Z_OBJPROP`; one whose mangling names another visibility
    /// than the declaration's is placed by `is_property_visibility_changed`,
    /// by the unmangled name among the class's properties, a parent's
    /// private one included as `ce->properties_info` holds it:
    ///
    /// ```php
    /// class P { private ?string $uri = null; function get() { return $this->uri; } }
    /// #[AllowDynamicProperties] class C extends P {}
    /// var_dump(unserialize('O:1:"C":1:{s:3:"uri";s:1:"X";}')->get());
    /// try { unserialize('O:1:"C":1:{s:3:"uri";i:5;}'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "X"
    /// Cannot assign int to property P::$uri of type ?string
    /// ```
    ///
    /// Any other key is a dynamic
    /// property under the key as written, created through
    /// [`Request::dynamic_property_creation`]'s two arms where the table
    /// does not hold it yet. `value` and `key` are the object's from here;
    /// what the slot held is released after the write.
    pub(crate) fn unserialize_property(
        &mut self,
        object: PhpObject,
        key: PhpString,
        value: Value,
    ) -> Result<bool, Thrown> {
        let class = object.class();
        let bytes = key.as_bytes().to_vec();
        let (mangled_class, name) = match bytes.strip_prefix(b"\0") {
            Some(rest) => match rest.iter().position(|b| *b == 0) {
                Some(end) => (Some(rest[..end].to_vec()), rest[end + 1..].to_vec()),
                None => (None, bytes.clone()),
            },
            None => (None, bytes.clone()),
        };
        let class_name = self.class_name_bytes(class);
        // `is_property_visibility_changed`'s `ZEND_ACC_VIRTUAL` arm: a
        // key the class's table holds a virtual property under has no
        // slot to take the value, and the whole unserialization fails.
        let reaches = match &mangled_class {
            None => true,
            Some(c) => c.as_slice() == b"*" || c.eq_ignore_ascii_case(&class_name),
        };
        if reaches
            && let Some(hooked) = self.hooked(class, &name)
            && hooked.is_virtual
        {
            let declaring = self.class_name(hooked.declaring);
            crate::api::raising::docref(
                self,
                Level::Warning,
                &format!(
                    "Cannot unserialize value for virtual property {declaring}::${}",
                    String::from_utf8_lossy(&name)
                ),
            );
            self.release(value);
            self.release(Value::string(key));
            return Ok(false);
        }
        let slot = self.unserialize_slot(object, &bytes);
        if let Some(at) = slot.and_then(|at| u32::try_from(at).ok()) {
            let z = value.into_zval();
            let o = object.heap();
            let plain = o.slot_bit(at, php_heap::PropBit::PlainRow);
            Heap::object_set_slot(&mut Hooks(self), o, at, z);
            // A plain row stays plain: php writes the table's row itself.
            o.set_slot_bit(at, php_heap::PropBit::PlainRow, plain);
            self.release(Value::string(key));
            return Ok(true);
        }
        let o = object.heap();
        let table_key = php_heap::Key::Str(key.heap());
        let held = o
            .properties()
            .is_some_and(|table| table.contains(table_key));
        if !held && let Err(thrown) = self.dynamic_property_creation(object, &name) {
            self.release(value);
            self.release(Value::string(key));
            return Err(thrown);
        }
        let table = self.separated_properties(o)?;
        self.array_update(table, table_key, value)?;
        self.release(Value::string(key));
        Ok(true)
    }

    /// The declared slot a serialized key places its value in, by its
    /// index among [`Request::placed_properties`]: the property the
    /// mangled name names, or the one of that name whose visibility
    /// changed since (`is_property_visibility_changed`). None where the
    /// table holds copies of the declared rows, whose rows are plain
    /// values, not php's `IS_INDIRECT` to a slot, and the value is
    /// written there with no type to hold to; a slot whose row alone is
    /// plain ([`PhpObject::update_row`]) is written so too.
    fn unserialize_slot(&self, object: PhpObject, key: &[u8]) -> Option<usize> {
        if object.heap().declared_rows() == php_heap::DeclaredRows::Copied {
            return None;
        }
        let class = object.class();
        let (mangled_class, name) = match key.strip_prefix(b"\0") {
            Some(rest) => match rest.iter().position(|b| *b == 0) {
                Some(end) => (Some(&rest[..end]), &rest[end + 1..]),
                None => (None, key),
            },
            None => (None, key),
        };
        let class_name = self.class_name_bytes(class);
        let exact = self.placed(class).rev().find(|(_, declaring, p)| {
            if *p.name.0 != *name {
                return false;
            }
            match (mangled_class, p.visibility) {
                (None, Visibility::Public) => true,
                (Some(c), Visibility::Protected) => c == b"*",
                (Some(c), Visibility::Private) => self.class_name_bytes(*declaring) == c,
                _ => false,
            }
        });
        exact.map(|(slot, _, _)| slot).or_else(|| {
            let reachable =
                mangled_class.is_none_or(|c| c == b"*" || c.eq_ignore_ascii_case(&class_name));
            if !reachable {
                return None;
            }
            // `ce->properties_info` holds a parent's private property
            // too, unless the class declares the name again, which the
            // later row of `placed` is.
            self.placed(class)
                .rev()
                .find(|(_, _, p)| *p.name.0 == *name)
                .map(|(slot, _, _)| slot)
        })
    }

    /// `process_nested_object_data`'s hold on a declared typed property
    /// once its value is read (`Z/ext/standard/var_unserializer.re`):
    /// the value held to the type under strict rules, a cell to every
    /// type its other sources hold it to, and the cell made one of this
    /// property's own; a refusal leaves the slot undefined.
    pub(crate) fn unserialize_verify(
        &mut self,
        object: PhpObject,
        key: &[u8],
    ) -> Result<(), Thrown> {
        let Some(index) = self.unserialize_slot(object, key) else {
            return Ok(());
        };
        let Ok(index) = u32::try_from(index) else {
            return Ok(());
        };
        let Some((declaring, ty, name)) = self
            .placed_property(object.class(), index)
            .and_then(|(declaring, p)| Some((declaring, p.ty.clone()?, p.name.0.to_vec())))
        else {
            return Ok(());
        };
        let o = object.heap();
        // A plain row is no `IS_INDIRECT`, so no property info is found
        // for it and nothing is held to a type
        // ([`PhpObject::update_row`]).
        if o.slot_bit(index, php_heap::PropBit::PlainRow) {
            return Ok(());
        }
        let held = Value::of_slot(o.slot(index));
        let checked = match held.as_php_ref() {
            Some(cell) => {
                let cell = cell.heap();
                let own = self
                    .ref_type_sources(cell)
                    .iter()
                    .any(|s| s.declaring == declaring && s.name == name);
                if own {
                    return Ok(());
                }
                crate::ops::verify_prop_assignable_by_ref(self, cell, &ty, declaring, &name, true)
                    .map(|coerced| {
                        if let Some(coerced) = coerced {
                            self.reference_set(cell, coerced);
                        }
                        self.ref_add_type_source(
                            cell,
                            RefSource {
                                declaring,
                                name,
                                ty,
                            },
                        );
                    })
            }
            None => crate::ops::verify_property_type(self, &held, &ty, declaring, &name, true).map(
                |coerced| {
                    if let Some(coerced) = coerced {
                        Heap::object_set_slot(&mut Hooks(self), o, index, coerced.into_zval());
                    }
                },
            ),
        };
        if checked.is_err() {
            Heap::object_set_slot(&mut Hooks(self), o, index, php_heap::Zval::UNDEF);
        }
        checked
    }

    /// `zend_cast_zval_to_object`'s array arm
    /// (`Z/Zend/zend_execute.h:225`), the `(object)` cast of an array:
    /// [`Request::symtable_to_proptable`] over it laid down as the new
    /// object's table, which is the array itself, one more owner of it,
    /// where its keys are all strings, and a copy of an immutable one,
    /// with no handler in the way, so a key a handler would refuse, a
    /// mangled one that opens with a NUL byte, is laid down as it is.
    /// The cast of an array that is kept costs the object's forty bytes,
    /// as php's does, and a write to either holder separates the two:
    ///
    /// ```php
    /// $a = []; foreach (['x', 'y', 'z'] as $k) { $a[$k] = str_repeat($k, 40); }
    /// $m = memory_get_usage(); $o = (object) $a; echo memory_get_usage() - $m, "\n";
    /// $o->x = 1; $a['y'] = 2; echo $a['x'][0], $o->y[0], "\n";
    /// ```
    /// ```expect
    /// 40
    /// xy
    /// ```
    pub(crate) fn properties_from_array(
        &mut self,
        object: PhpObject,
        array: PhpArray,
    ) -> Result<(), Thrown> {
        let table = self.symtable_to_proptable(array.heap())?;
        let table = if table.is_immutable() {
            self.array_dup(table)?
        } else {
            table
        };
        let o = object.heap();
        if let Some(old) = self.heap.object_take_properties(o) {
            self.release(Value::array(PhpArray::of(old)));
        }
        self.heap.object_set_properties(o, table);
        Ok(())
    }

    /// The two arms php takes before it lays a name the class did not
    /// declare into the dynamic table, in
    /// `zend_std_write_property` and in `zend_std_get_property_ptr_ptr`
    /// alike (`Z/Zend/zend_object_handlers.c:1288`, `:1522`): a class
    /// with `ZEND_ACC_NO_DYNAMIC_PROPERTIES` is
    /// `zend_forbidden_dynamic_property`'s `Error`, and a class without
    /// `ZEND_ACC_ALLOW_DYNAMIC_PROPERTIES` is
    /// `zend_deprecated_dynamic_property`'s 8.2 deprecation. Only a
    /// creation takes them: a name the table already holds is written
    /// with neither.
    ///
    /// php's raiser holds one more owner of the object across the
    /// deprecation (`GC_ADDREF(obj)`), and where the handler the
    /// deprecation reached gave up every other one, the object is
    /// destroyed and the creation refused with the same `Error`, unless
    /// the handler threw.
    ///
    /// ```php
    /// set_error_handler(function ($code, $msg) { echo "Err: $msg\n"; $GLOBALS['a'] = null; });
    /// $a = new class {};
    /// try { [&$a->y]; } catch (Throwable $ex) { echo "Exception: ", $ex->getMessage(), "\n"; }
    /// var_dump($a);
    /// ```
    /// ```expect
    /// Err: Creation of dynamic property class@anonymous::$y is deprecated
    /// Exception: Cannot create dynamic property class@anonymous::$y
    /// NULL
    /// ```
    pub(crate) fn dynamic_property_creation(
        &mut self,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        use php_unit::decl::DynamicProperties;
        let class = object.class();
        let Some(flags) = self.class_flags(class) else {
            return Ok(());
        };
        let carries = flags.dynamic_properties;
        if carries == DynamicProperties::Allowed {
            return Ok(());
        }
        let shown = String::from_utf8_lossy(name).into_owned();
        let class_name = self.class_name(class);
        if carries == DynamicProperties::Forbidden {
            return Err(crate::api::raising::throw_error(
                self,
                "Error",
                &format!("Cannot create dynamic property {class_name}::${shown}"),
            ));
        }
        let held = Owner::take(self, object.as_value());
        crate::api::raising::deprecated(
            self,
            &format!("Creation of dynamic property {class_name}::${shown} is deprecated"),
        );
        if object.as_value().refcount() > 1 {
            held.let_go(self);
            return Ok(());
        }
        held.let_go(self);
        none_pending(self)?;
        Err(crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Cannot create dynamic property {class_name}::${shown}"),
        ))
    }

    /// `ZEND_FE_RESET_R`'s separation of a shared property table
    /// (`Z/Zend/zend_vm_def.h:6903`): the loop walks the object's own
    /// table, so one a cast shares is duplicated first and the object
    /// takes the copy, php's `zend_array_dup` under
    /// `GC_REFCOUNT(zobj->properties) > 1`. An object that has no table
    /// has nothing to separate and is given none, as php's arm is under
    /// `Z_OBJ_P(array_ptr)->properties` being there at all.
    pub(crate) fn separate_object_properties(&mut self, object: PhpObject) -> Result<(), Thrown> {
        let o = object.heap();
        if o.properties().is_some() {
            self.separated_properties(o)?;
        }
        Ok(())
    }

    /// The start of a `foreach` over an object whose class has no
    /// `get_iterator`, the object arm `FE_RESET_R` and `FE_RESET_RW`
    /// share (`Z/Zend/zend_vm_def.h:6903-6934`): the property table the
    /// walk goes over made the object's own, separated where a cast
    /// shares it and laid down where the object has none, which is php's
    /// `get_properties` materialising it; then, where the object has a
    /// property at all, declared or dynamic, the hash iterator the walk
    /// is registered at (`zend_hash_iterator_add(properties, 0)`), whose
    /// index is the loop variable's `Z_FE_ITER`. `None` is the empty
    /// table php jumps past the loop over with `Z_FE_ITER` at `-1`.
    pub(crate) fn object_walk_start(&mut self, object: PhpObject) -> Result<Option<u32>, Thrown> {
        // `zend_lazy_object_init`: a lazy object is initialised before
        // the walk, which goes over what that answers.
        let object = self.lazy_init(object)?;
        self.separate_object_properties(object)?;
        let o = object.heap();
        let table = self.heap.object_properties(o);
        let table = self.fatal_of(table)?;
        // `Z_OBJPROP_P` of a class whose `get_properties` answers its
        // own rows: the walk stands on the object's table as any does,
        // and goes over what the class answers.
        let handlers = self.object_handlers(o);
        if handlers.properties_overloaded() {
            let answered = handlers.get_properties(self, object)?;
            if walk::rows(self, answered).next().is_none() {
                return Ok(None);
            }
            return Ok(Some(self.heap.iterator_add(table, 0)));
        }
        let slots = match table.declared_rows() {
            php_heap::DeclaredRows::Slots => self.class_slots(object.class()),
            php_heap::DeclaredRows::Copied => 0,
        };
        // `zend_hooked_object_get_iterator` lays its iterator whatever
        // the object holds.
        let hooked = self.class_has_hooks(object.class());
        if !hooked && slots == 0 && table.is_empty() {
            return Ok(None);
        }
        let at = self.heap.iterator_add(table, 0);
        if hooked && let Err(thrown) = self.hooked_walk_start(object, at) {
            self.heap.iterator_del(at);
            return Err(thrown);
        }
        Ok(Some(at))
    }

    /// `zend_fe_fetch_object_helper` (`Z/Zend/zend_vm_def.h:7058`) over
    /// an object whose class has no `get_iterator`, and the object arm
    /// of `FE_FETCH_RW`: the next property the executing scope may see
    /// from where iterator `idx` stands, with its key and where it sits,
    /// the iterator left one past it ([`walk::object_step`]), moved onto
    /// the object's table at its pointer first where the loop's variable
    /// now holds another object. The order is `Z_OBJPROP`'s: the
    /// class's declared instance properties in declaration order, which
    /// php's table carries as its first buckets and this heap walks by
    /// the iterator's lead ([`php_heap::Heap::iterator_lead`]), then the
    /// dynamic table from the iterator's own position, which every write
    /// to the table moves as php's writes move it. A slot nothing has
    /// written is stepped over as php steps over an `IS_UNDEF`
    /// behind an `IS_INDIRECT`, and one the executing scope may not
    /// reach is stepped over as php steps over a mangled key
    /// `zend_check_property_access` refuses. `None` is nothing left,
    /// which is the fetch's jump past the loop. The key is handed on
    /// unmade ([`WalkKey`]) and made by [`Request::walked_key`] only
    /// where the loop keeps one, as `RETURN_VALUE_USED` guards php's: a
    /// public property's the interned name, a protected or a private
    /// one's a string of its own ([`Request::walk_key`]), so a loop that
    /// holds the last key while the next is made peaks at both. Asking
    /// whether the scope reaches a protected or a private slot costs the
    /// copy of its name `zend_check_property_access` makes and frees
    /// ([`Request::access_name_copy`]), key or none.
    ///
    /// ```php
    /// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); $p = memory_get_peak_usage(); $u = memory_get_usage(); echo $p - $s, ' ', $u - $s, "\n"; }
    /// class P3 { public $a = 1; private $pp = 3; protected $qq = 4; function w() { $o = []; foreach ($this as $k => $v) $o[] = $k; return 1; } function z() { foreach ($this as $k => $v) {} return 1; } function r() { foreach ($this as $k => &$v) {} return 1; } function n() { foreach ($this as $v) {} return 1; } }
    /// $p = new P3;
    /// pk(fn () => $p->w());
    /// pk(fn () => $p->z());
    /// pk(fn () => $p->r());
    /// pk(fn () => $p->n());
    /// $o = (object)["\0A\0b" => 42, "\0*\0c" => 24, "d" => 1];
    /// pk(function () use ($o) { foreach ($o as $k => $v) {} return 1; });
    /// pk(function () use ($o) { foreach ($o as $v) {} return 1; });
    /// ```
    /// ```expect
    /// 280 0
    /// 64 0
    /// 96 0
    /// 32 0
    /// 64 0
    /// 0 0
    /// ```
    pub(crate) fn object_walk(
        &mut self,
        object: PhpObject,
        idx: u32,
        by_ref: bool,
    ) -> Result<Option<(WalkKey, Walked)>, Thrown> {
        let o = object.heap();
        let class = object.class();
        if self.object_handlers(o).properties_overloaded() {
            return self.answered_walk(object, idx);
        }
        // `zend_hash_iterator_pos(.., Z_OBJPROP_P(array))`: the table laid
        // down as php's `rebuild_object_properties` lays it, and the
        // iterator moved onto it at its pointer where the loop's
        // variable now holds another object, before its place is read.
        let laid = self.heap.object_property_table(o);
        let table = self.fatal_of(laid)?;
        self.heap.iterator_pos(idx, table.table());
        // `zend_hooked_object_get_iterator`: a class with a hooked
        // property walks its declared rows in the table's order, each
        // hooked one read through its hook, and then the dynamic rows.
        let stepped = if self.class_has_hooks(class) {
            let mut lead = self.heap.iterator_lead(idx) as usize;
            let found = self.hooked_object_walk(idx, &mut lead, by_ref);
            self.heap
                .iterator_set_lead(idx, u32::try_from(lead).unwrap_or(u32::MAX));
            if let Some((key, walked)) = found? {
                return Ok(Some((WalkKey::Held(key.into_key()), walked)));
            }
            walk::Stepped::Dynamic
        } else {
            walk::Stepped::Positions
        };
        // A row the executing scope may not reach is stepped over
        // (`zend_check_property_access`): a declared one by its
        // declaration, a dynamic one whose name the scope finds declared
        // and not public.
        let found = walk::object_step(
            self,
            idx,
            (table, stepped),
            walk::Advance::Past,
            |req, row| match row {
                TableRow::Slot(slot) => {
                    let Some((declaring, p)) = req.placed_property(class, *slot) else {
                        return Ok(false);
                    };
                    let (len, visibility) = (p.name.0.len(), p.visibility);
                    let visible = req.property_row_visible(class, &p.name.0, Some(declaring));
                    req.access_name_copy(len, visibility)?;
                    Ok(visible)
                }
                TableRow::Entry(php_heap::Key::Str(name)) => {
                    Ok(req.property_row_visible(class, name.bytes(), None))
                }
                TableRow::Entry(php_heap::Key::Long(_)) => Ok(true),
            },
        )?;
        match found {
            None => Ok(None),
            Some((_, TableRow::Slot(slot), _)) => {
                let Some((_, p)) = self.placed_property(class, slot) else {
                    return Err(crate::api::raising::fatal(
                        self,
                        "InternalError: an object walk's slot with no declaration",
                    ));
                };
                let key = WalkKey::Declared(p.name.0.clone(), p.visibility);
                Ok(Some((key, Walked::Slot(slot))))
            }
            Some((_, TableRow::Entry(key), _)) => Ok(Some((
                WalkKey::Held(Owner::take(self, Key::of(key))),
                Walked::Dynamic(key),
            ))),
        }
    }

    /// [`Request::object_walk`] over an object whose class answers its
    /// own rows, `zend_fe_fetch_object_helper` over `Z_OBJPROP_P`, which
    /// is the class's `get_properties` asked again at every fetch: the
    /// next row from the iterator's lead, the rows' index, that the
    /// executing scope may see, a row the class answered for a property
    /// the object holds walked as that slot or that dynamic entry, so a
    /// walk by reference binds it where the std walk does, and any other
    /// row, a `DateInterval`'s field, as the value the class answered.
    ///
    /// ```php
    /// foreach (new DateInterval('P1D') as $k => $v) { echo $k, '=', var_export($v, true), "\n"; }
    /// class D extends DateTime { public $own = 1; protected $hidden = 2; }
    /// $d = new D('2020-01-01');
    /// foreach ($d as $k => &$v) { $v = 5; }
    /// unset($v);
    /// var_dump(get_object_vars($d), $k);
    /// $i = new DateInterval('P2D');
    /// foreach ($i as $k => &$v) { if ($k === 'd') { $v = 9; } }
    /// unset($v);
    /// var_dump($i->d);
    /// ```
    /// ```expect
    /// y=0
    /// m=0
    /// d=1
    /// h=0
    /// i=0
    /// s=0
    /// f=0.0
    /// invert=0
    /// days=false
    /// from_string=false
    /// array(1) {
    ///   ["own"]=>
    ///   int(5)
    /// }
    /// string(3) "own"
    /// int(2)
    /// ```
    ///
    /// A declared row the table holds mangled is asked about through a
    /// copy of its name ([`Request::access_name_copy`]) and keyed by a
    /// string of its own ([`WalkKey::Declared`]), as the std walk is:
    ///
    /// ```php
    /// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); $p = memory_get_peak_usage(); $u = memory_get_usage(); echo $p - $s, ' ', $u - $s, "\n"; }
    /// class D extends DateTime { public $own = 1; protected $hidden = 2; private $pv = 3;
    ///   function z() { foreach ($this as $k => $v) {} return 1; } function n() { foreach ($this as $v) {} return 1; } }
    /// $d = new D('2020-01-01');
    /// pk(fn () => $d->z()); pk(fn () => $d->n());
    /// pk(function () use ($d) { foreach ($d as $k => $v) {} return 1; });
    /// ```
    /// ```expect
    /// 64 0
    /// 32 0
    /// 32 0
    /// ```
    fn answered_walk(
        &mut self,
        object: PhpObject,
        idx: u32,
    ) -> Result<Option<(WalkKey, Walked)>, Thrown> {
        let class = object.class();
        let handlers = self.object_handlers(object.heap());
        let mut at = self.heap.iterator_lead(idx);
        let found = match handlers.get_properties(self, object)? {
            // The object's own table, a `DateInterval`'s fields among its
            // dynamic rows: a slot walked as that slot and a dynamic row
            // as that entry, so a walk by reference binds it.
            Table::Object(table) => loop {
                let end = table.positions().used();
                if at >= end {
                    break None;
                }
                let pos = at;
                at = at.saturating_add(1);
                let Some(((name, _, declared), row)) = self.table_row_named(&table, pos)? else {
                    continue;
                };
                let walked = match row {
                    TableRow::Slot(slot) => Walked::Slot(slot),
                    TableRow::Entry(key) => Walked::Dynamic(key),
                };
                // A declared row is asked about through a copy of its
                // name, and an integer key is no name
                // `zend_check_property_access` is asked about.
                let key = match (&name, declared) {
                    (Key::Str(text), Some((declaring, visibility))) => {
                        let visible =
                            self.property_row_visible(class, text.as_bytes(), Some(declaring));
                        self.access_name_copy(text.as_bytes().len(), visibility)?;
                        if !visible {
                            continue;
                        }
                        WalkKey::Declared(text.as_bytes().into(), visibility)
                    }
                    (Key::Str(text), None)
                        if !self.property_row_visible(class, text.as_bytes(), None) =>
                    {
                        continue;
                    }
                    _ => WalkKey::Held(Owner::take(self, name)),
                };
                break Some((key, walked));
            },
            // A table the class keeps in place of the object's: each row
            // the value it answered.
            Table::Array(array) => {
                let row = walk::rows(self, array)
                    .enumerate()
                    .skip(usize::try_from(at).unwrap_or(usize::MAX))
                    .find(|(_, (key, _))| match key {
                        Key::Str(name) => self.property_row_visible(class, name.as_bytes(), None),
                        Key::Int(_) => true,
                    });
                row.map(|(i, (key, value))| {
                    at = u32::try_from(i).unwrap_or(u32::MAX).saturating_add(1);
                    self.addref(&value);
                    let key = WalkKey::Held(Owner::take(self, key));
                    (key, Walked::Answered(value))
                })
            }
        };
        self.heap.iterator_set_lead(idx, at);
        Ok(found)
    }

    /// The row at position `pos` of `table` that holds a value, as a
    /// reader that unmangles reads it: a declared slot under its name as
    /// declared, beside the class that declared it and its visibility,
    /// and a dynamic row under its key, with what stands there; `None`
    /// for a slot nothing holds and a tombstone. The `Err` is the memory
    /// limit refusing the intern of a name.
    fn table_row_named(
        &mut self,
        table: &ObjectTable,
        pos: u32,
    ) -> Result<Option<(PropertyRow, TableRow)>, Thrown> {
        let Some((row, z)) = table.positions().row_at(pos) else {
            return Ok(None);
        };
        if z.is_undef() {
            return Ok(None);
        }
        let value = Value::of_slot(z);
        match row {
            TableRow::Entry(key) => Ok(Some(((Key::of(key), value, None), row))),
            TableRow::Slot(slot) => {
                let class = table.owner().class();
                let Some((declaring, decl)) = self.placed_property(class, slot) else {
                    return Ok(None);
                };
                let (visibility, name) = (decl.visibility, decl.name.0.clone());
                let name = self.kept_string(&name)?;
                let row = (Key::Str(name), value, Some((declaring, visibility)));
                Ok(Some((row, TableRow::Slot(slot))))
            }
        }
    }

    /// The key `FE_FETCH` hands a loop over an object's table for the row
    /// [`Request::object_walk`] found, where the loop keeps one (`keyed`),
    /// and none otherwise, a key the walk held given up: a declared
    /// property's as [`Request::walk_key`] makes it, a held name as it
    /// is, and a mangled one, which a cast of an array can leave in the
    /// dynamic table, a string of its own with its class part dropped
    /// (`zend_unmangle_property_name_ex`).
    ///
    /// ```php
    /// $obj = (object)["\0A\0b" => 42, "\0*\0c" => 24, "d" => 1];
    /// foreach ($obj as $k => $v) { var_dump($k, $v); }
    /// ```
    /// ```expect
    /// string(1) "b"
    /// int(42)
    /// string(1) "c"
    /// int(24)
    /// string(1) "d"
    /// int(1)
    /// ```
    pub(crate) fn walked_key(
        &mut self,
        key: WalkKey,
        keyed: bool,
    ) -> Result<Option<Owner<Key>>, Thrown> {
        let key = match key {
            WalkKey::Held(key) if keyed => key,
            WalkKey::Declared(name, visibility) if keyed => {
                return Ok(Some(self.walk_key(&name, visibility)?.into_key()));
            }
            WalkKey::Held(key) => {
                key.give_up(self);
                return Ok(None);
            }
            WalkKey::Declared(..) => return Ok(None),
        };
        let Key::Str(name) = *key else {
            return Ok(Some(key));
        };
        let crate::natives::Unmangled::Mangled { prop, .. } =
            crate::natives::unmangle_property_name(name.as_bytes())
        else {
            return Ok(Some(key));
        };
        let made = self.string_init(prop);
        key.give_up(self);
        Ok(Some(Owner::adopt(made?).into_key()))
    }

    /// What [`Request::object_walk`] found at a place: the value, a view
    /// with no owner taken and a reference followed where `deref`.
    pub(crate) fn walked_value(object: PhpObject, walked: Walked) -> Value {
        let o = object.heap();
        let z = match walked {
            Walked::Slot(slot) => o.slot(slot),
            Walked::Dynamic(key) => o
                .properties()
                .and_then(|table| table.get(key))
                .unwrap_or(php_heap::Zval::UNDEF),
            Walked::Hooked(value) | Walked::Answered(value) => return value,
        };
        Value::of_slot(z)
    }

    /// `FE_FETCH_RW`'s bind over an object's property
    /// (`Z/Zend/zend_vm_def.h:7331-7357`, `:7434-7439`): the cell the
    /// place holds, or one made there over the value it held
    /// (`ZVAL_NEW_EMPTY_REF`), the place keeping the owner it had. A
    /// declared `readonly` slot is php's `Error` and no cell; a typed
    /// one puts its type on the cell it makes (`ZEND_REF_ADD_TYPE_SOURCE`),
    /// so a write through the loop's variable is held to it. The
    /// `readonly` `Error` names the property by the bucket's key, which
    /// for a protected or a private one is mangled and prints as nothing:
    ///
    /// ```php
    /// class A { public readonly int $a; function __construct() { $this->a = 1; } }
    /// class B { protected readonly int $b; function __construct() { $this->b = 2; } }
    /// class C { private readonly int $c; function __construct() { $this->c = 3; } function f() { try { foreach ($this as $k => &$v) {} } catch (Error $e) { echo $e->getMessage(), "\n"; } } }
    /// foreach ([new A, new B] as $o) { (function () { try { foreach ($this as $k => &$v) {} } catch (Error $e) { echo $e->getMessage(), "\n"; } })->call($o); }
    /// (new C)->f();
    /// ```
    /// ```expect
    /// Cannot acquire reference to readonly property A::$a
    /// Cannot acquire reference to readonly property B::$
    /// Cannot acquire reference to readonly property C::$
    /// ```
    pub(crate) fn walked_reference(
        &mut self,
        object: PhpObject,
        walked: Walked,
    ) -> Result<PhpRef, Thrown> {
        let o = object.heap();
        let held = Request::walked_value(object, walked);
        if let Some(cell) = held.as_php_ref() {
            return Ok(cell);
        }
        match walked {
            Walked::Slot(slot) => {
                let declared = self
                    .placed_property(object.class(), slot)
                    .map(|(declaring, p)| {
                        let readonly = p.is_readonly.then_some(p.visibility);
                        (declaring, readonly, p.name.0.to_vec(), p.ty.clone())
                    });
                let Some((declaring, readonly, name, ty)) = declared else {
                    return self.fatal_of(Err(Fatal::error(format!(
                        "InternalError: a walk over slot {slot}, which the class does not declare"
                    ))));
                };
                // php names it by the bucket's key, `ZSTR_VAL(p->key)`, which
                // a protected or a private property's leading `\0` ends.
                if let Some(visibility) = readonly {
                    let public = visibility == Visibility::Public;
                    let shown = if public { &name[..] } else { b"" };
                    let operation = "acquire reference to";
                    return Err(self.readonly_modification(declaring, shown, operation));
                }
                let cell = self.reference(held)?;
                if let Some(place) = o.slot_ref(slot) {
                    place.write(cell.as_value().to_zval());
                }
                if let Some(ty) = ty {
                    self.ref_add_type_source(
                        cell.heap(),
                        RefSource {
                            declaring,
                            name,
                            ty,
                        },
                    );
                }
                Ok(cell)
            }
            Walked::Dynamic(key) => {
                let place = o.properties().and_then(|table| table.value_ref(key));
                let cell = self.reference(held)?;
                if let Some(place) = place {
                    place.write(cell.as_value().to_zval());
                }
                Ok(cell)
            }
            // A hook that answered no reference has no place to bind; the
            // walk refused one whose `get` hook returns none before it
            // ran it ([`Request::hooked_object_walk`]).
            // A row the class answered and the object holds nowhere, a
            // `DateInterval`'s field: php binds it in the table
            // `get_properties` filled, which the next fetch fills again,
            // so the cell stands on its own.
            Walked::Answered(value) => self.reference(value),
            Walked::Hooked(value) => {
                self.release(value);
                self.fatal_of(Err(Fatal::error(
                    "InternalError: a hooked property's value bound by reference that its hook did not return by reference",
                )))
            }
        }
    }

    /// `zend_std_get_property_ptr_ptr`'s dynamic arm under `BP_VAR_W`
    /// (`Z/Zend/zend_object_handlers.c:1491`): the dynamic table
    /// separated where a cast shares it and the name looked up in it;
    /// where it is not there, php's two arms over the creation
    /// ([`Request::dynamic_property_creation`]) and then
    /// `zend_hash_add(zobj->properties, name, &EG(uninitialized_zval))`,
    /// which is `IS_NULL` (`zend_execute_API.c:132`) and is what the
    /// `ASSIGN*` beside this writes through, and what a caught `Error`
    /// leaves the property holding. The name is given up here, as
    /// [`Request::write_property`] gives it up.
    ///
    /// The `ZvalRef` names a bucket of that table, so it is a name only
    /// until the table is written again ([`php_heap::Arr::value_ref`]);
    /// the one opline between this and the write through it is the
    /// `ASSIGN*` php pairs it with, which inserts nothing.
    ///
    /// `read` is php's `type`: under [`PropertyRead::Warning`], which is
    /// `BP_VAR_RW` and `BP_VAR_R`, the `Undefined property` warning
    /// stands between the creation's deprecation and the entry
    /// (`Z/Zend/zend_object_handlers.c:1530`), which is the order php
    /// raises the two in and the reverse of the order the pair
    /// [`Request::read_property`] and [`Request::write_property`]
    /// raises them in over the same name. A name the table already
    /// holds raises neither, php returning before both. The entry is
    /// added after the warning, to the table the handler it reached left,
    /// so a value that handler wrote under the name is what the compound
    /// assignment reads.
    ///
    /// ```php
    /// #[AllowDynamicProperties]
    /// class C {
    ///     function error($_, $msg) { echo $msg, "\n"; $this->a = 12345; }
    /// }
    /// $c = new C;
    /// set_error_handler([$c, 'error']);
    /// $c->a %= 10;
    /// var_dump($c->a);
    /// ```
    /// ```expect
    /// Undefined property: C::$a
    /// int(5)
    /// ```
    pub(crate) fn dynamic_property_ref(
        &mut self,
        object: PhpObject,
        name: PhpString,
        read: PropertyRead,
    ) -> Result<Option<php_heap::ZvalRef>, Thrown> {
        let o = object.heap();
        let key = php_heap::Key::Str(name.heap());
        let held = o.properties().is_some_and(|table| table.contains(key));
        if !held && let Err(thrown) = self.dynamic_property_creation(object, name.as_bytes()) {
            self.release(Value::string(name));
            return Err(thrown);
        }
        if !held && read == PropertyRead::Warning {
            let class = self.class_name(object.class());
            let shown = String::from_utf8_lossy(name.as_bytes()).into_owned();
            crate::api::raising::warning(self, &format!("Undefined property: {class}::${shown}"));
        }
        // `zend_hash_add(zobj->properties, name, &EG(uninitialized_zval))`
        // after the warning: the table as the handler the warning reached
        // left it, and a name it wrote kept as it wrote it.
        let table = self.separated_properties(o)?;
        if !table.contains(key) {
            self.array_update(table, key, Value::null())?;
        }
        self.release(Value::string(name));
        Ok(table.value_ref(key))
    }

    /// `EXPECTED(!zobj->ce->__get) || UNEXPECTED((*zend_get_property_guard(zobj,
    /// name)) & IN_GET)` (`Z/Zend/zend_object_handlers.c:1489`) read the
    /// other way round: whether `zend_std_get_property_ptr_ptr` hands
    /// out no place for `name` and leaves the site to `read_property`
    /// and `write_property`, which is a class with a `__get` that the
    /// guard is not already holding.
    pub(crate) fn overloaded_place(&self, object: PhpObject, name: &[u8]) -> bool {
        self.serves_accessor(object.class(), b"__get")
            && (self.guard_word(object, name) & IN_GET == 0
                || self.wrong_name(object.class(), name))
    }

    /// The rows of `table` that hold a value, each beside the declared
    /// slot it is, the class that declared what stands there and its
    /// visibility, or none for a dynamic row, in php's positions: the
    /// slots in slot order, then the dynamic table, a declared row under
    /// its name as declared. Views, no owner taken.
    pub(crate) fn property_rows(
        &mut self,
        table: &ObjectTable,
    ) -> Result<Vec<PropertyRow>, Thrown> {
        let mut rows = Vec::new();
        for pos in 0..table.positions().used() {
            if let Some((row, _)) = self.table_row_named(table, pos)? {
                rows.push(row);
            }
        }
        Ok(rows)
    }

    /// The properties in order, the declared slots that hold a value
    /// then the dynamic table, which is the order
    /// `rebuild_object_properties` lays the table out in. Views, a
    /// declared name the string php's `prop_info->name` is
    /// ([`Request::kept_string`]), which a caller that keeps it takes
    /// an owner of; the `Err` is the memory limit. A
    /// dynamic row is keyed as the table holds it, so an integer key,
    /// which `object_properties_load` and `zend_hash_copy` write with
    /// `zend_hash_index_update`, stays an integer beside the string that
    /// spells it, and a property access, which names a string, misses it.
    ///
    /// ```php
    /// $g = unserialize('C:3:"GMP":23:{s:1:"2";a:1:{i:46;i:7;}}');
    /// @$g->{46} = 8;
    /// var_dump($g);
    /// foreach ($g as $k => $v) var_dump($k);
    /// var_dump(get_object_vars(clone $g), (array)$g, $g->{46});
    /// ```
    /// ```expect
    /// object(GMP)#1 (3) {
    ///   [46]=>
    ///   int(7)
    ///   ["46"]=>
    ///   int(8)
    ///   ["num"]=>
    ///   string(1) "2"
    /// }
    /// int(46)
    /// string(2) "46"
    /// array(1) {
    ///   [46]=>
    ///   int(8)
    /// }
    /// array(1) {
    ///   [46]=>
    ///   int(8)
    /// }
    /// int(8)
    /// ```
    pub(crate) fn object_entries(&mut self, o: Obj) -> Result<Vec<(Key, Value)>, Thrown> {
        let mut entries = Vec::new();
        // A table that holds copies of the declared rows is read alone,
        // as php reads the plain values its `zend_array_dup` left.
        let declared: Vec<Box<[u8]>> = match o.declared_rows() {
            php_heap::DeclaredRows::Slots => self
                .placed(ClassId(o.class()))
                .map(|(_, _, p)| p.name.0.clone())
                .collect(),
            php_heap::DeclaredRows::Copied => Vec::new(),
        };
        for (i, name) in declared.iter().enumerate() {
            let z = o.slot(u32::try_from(i).unwrap_or(u32::MAX));
            if z.is_undef() {
                continue;
            }
            let name = self.kept_string(name)?;
            entries.push((Key::Str(name), Value::of_slot(z)));
        }
        if let Some(table) = o.properties() {
            for (k, z) in &table {
                entries.push((Key::of(k), Value::of_slot(z)));
            }
        }
        Ok(entries)
    }

    /// `zend_array_count` over the property table
    /// `zend_std_get_properties` builds for `object` (`Z/Zend/zend_hash.c:519`):
    /// one row per declared slot and one per dynamic property, the
    /// declared slots counted again, the empty ones left out, only where
    /// the table is marked `HASH_FLAG_HAS_EMPTY_IND`, and the mark
    /// cleared where that finds none empty. So a slot `unset` empties
    /// still counts inside the release that empties it, before the mark
    /// is set.
    ///
    /// ```php
    /// class N { public $p = null; public $c = [];
    ///     function __destruct() { var_dump($this); unset($this->c); } }
    /// $a = new N; $a->c[] = new N; $a->c[0]->p = $a;
    /// ```
    /// ```expect
    /// object(N)#1 (2) {
    ///   ["p"]=>
    ///   NULL
    ///   ["c"]=>
    ///   array(1) {
    ///     [0]=>
    ///     object(N)#2 (2) {
    ///       ["p"]=>
    ///       *RECURSION*
    ///       ["c"]=>
    ///       array(0) {
    ///       }
    ///     }
    ///   }
    /// }
    /// object(N)#2 (2) {
    ///   ["p"]=>
    ///   object(N)#1 (2) {
    ///     ["p"]=>
    ///     NULL
    ///   }
    ///   ["c"]=>
    ///   array(0) {
    ///   }
    /// }
    /// ```
    pub(crate) fn property_count(&mut self, object: PhpObject) -> Result<usize, Thrown> {
        let o = object.heap();
        let built = self.heap.object_properties(o);
        let table = self.fatal_of(built)?;
        // A copy holds no `IS_INDIRECT`, so it counts its rows.
        if table.declared_rows() == php_heap::DeclaredRows::Copied {
            return Ok(table.len());
        }
        let declared = self.placed_properties(object.class()).len();
        let total = declared + table.len();
        if !table.has_empty_ind() {
            return Ok(total);
        }
        let held = (0..declared)
            .filter(|at| !o.slot(u32::try_from(*at).unwrap_or(u32::MAX)).is_undef())
            .count();
        let counted = held + table.len();
        if counted == total {
            table.set_has_empty_ind(false);
        }
        Ok(counted)
    }

    /// `zend_std_has_property`: the declared slot where the scope may
    /// reach it and it holds a value, else the dynamic table's entry;
    /// then what `test` asks of the value: that it is there, that it is
    /// not `null`, or that it is truthy. A slot the scope may not reach
    /// or nothing wrote is the class's `__isset`, whose answer's truth
    /// is the answer; php places the name silently here whatever the
    /// class declares (`zend_get_property_offset(ce, name, 1, ...)`),
    /// so the property the scope may not reach reaches the accessor and
    /// never the `Error`. Under [`PropertyTest::Exists`],
    /// `property_exists()`'s `ZEND_PROPERTY_EXISTS`, no accessor is
    /// called at all.
    ///
    /// Under [`PropertyTest::NotEmpty`], `empty($o->p)`, a `__isset`
    /// that said yes is followed by `__get`, whose value's truth is the
    /// answer, and a class with no `__get` answers `false`; both are
    /// served under the guard the `__isset` still holds, so an
    /// accessor that asks about its own name answers for itself.
    ///
    /// ```php
    /// class M {
    ///     public $kept = 1;
    ///     public function __isset($k) { echo "isset $k\n"; return $k === "y"; }
    ///     public function __get($k) { echo "get $k\n"; return 0; }
    /// }
    /// $m = new M;
    /// var_dump(isset($m->kept), isset($m->y), isset($m->nope));
    /// var_dump(empty($m->y), empty($m->nope));
    /// ```
    /// ```expect
    /// isset y
    /// isset nope
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// isset y
    /// get y
    /// isset nope
    /// bool(true)
    /// bool(true)
    /// ```
    pub(crate) fn has_property(
        &mut self,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        let class = object.class();
        let o = object.heap();
        if let Some(hooked) = self.hooked(class, name)
            && self.hooked_accessible(&hooked)
        {
            return self.has_hooked(object, name, test, hooked);
        }
        // php's `value`, the slot the placement found, which keeps a
        // lazy object from initialising unless the slot itself waits.
        let mut placed_slot = None;
        let value = match self.declared_slot(class, name) {
            Some(placed) => {
                if self.reaches(&placed) {
                    placed_slot = Some(placed.slot);
                    let z = o.slot(placed.slot);
                    (!z.is_undef()).then(|| Value::of_slot(z))
                } else {
                    None
                }
            }
            // A name the scope finds no declaration of is the dynamic
            // table's alone, a parent's private slot of the same name
            // being none of its business.
            None if Self::bad_property_name(name) => None,
            None => Self::dynamic_prop(o, name),
        };
        if let Some(value) = value {
            let value = value.deref();
            return Ok(match test {
                PropertyTest::Exists => true,
                PropertyTest::Isset => value.tag() != TypeTag::Null,
                PropertyTest::NotEmpty => crate::api::coerce::truthy(self, &value)?,
            });
        }
        // `IS_PROP_UNINIT`: "Skip __isset() for uninitialized typed
        // properties", which answer no whatever the test; a slot that
        // waits on a lazy initializer is `IS_PROP_LAZY` first, which
        // initialises the object below.
        if placed_slot.is_some_and(|slot| o.slot_uninit(slot) && !self.lazy_slot(o, slot)) {
            return Ok(false);
        }
        // `lazy_init:`, which a slot that waits reaches past `__isset`
        // and a name the placement found no slot for reaches where no
        // accessor answers it.
        let initialises = |req: &Request| match placed_slot {
            Some(slot) => req.lazy_slot(o, slot),
            None => o.laziness() != php_heap::Laziness::Eager,
        };
        if placed_slot.is_some_and(|slot| self.lazy_slot(o, slot)) {
            return self.lazy_has(object, name, test);
        }
        // GH-21478: a proxy that forwards, asked from inside the
        // instance's own `__isset` for this name, asks the instance.
        if let Some(instance) = self.lazy_instance(o)
            && self.guard_word(instance, name) & IN_ISSET != 0
        {
            return self.has_property(instance, name, test);
        }
        let issetter = self.magic_accessor(class, b"__isset");
        if issetter.is_some() && test == PropertyTest::Exists {
            return Ok(false);
        }
        let Some(issetter) = issetter else {
            if initialises(self) {
                return self.lazy_has(object, name, test);
            }
            return Ok(false);
        };
        if self.guard_word(object, name) & IN_ISSET != 0 {
            if initialises(self) {
                return self.lazy_has(object, name, test);
            }
            return Ok(false);
        }
        // php holds `IN_ISSET` across both calls, so the `__get` the
        // `empty` arm asks for runs with the `__isset`'s guard still
        // standing, and the object across both (`GC_ADDREF(zobj)`,
        // `OBJ_RELEASE` after the guard is down), so an `__isset` that
        // gives up every other owner does not free the object the
        // `__get` is served on.
        self.guard_take(object, name, IN_ISSET);
        let held = Owner::take(self, object.as_value());
        let answered = self.isset_then_truth(object, name, test, issetter);
        self.guard_drop(object, name, IN_ISSET);
        held.give_up(self);
        answered
    }

    /// `zend_std_has_property`'s `/* have issetter - try with it! */`
    /// arm, under the guard and the hold its caller takes: the truth of
    /// what the `__isset` answered, and under `empty` the truth of what
    /// the `__get` answers after a yes; a class with no `__get`, or one
    /// whose guard is up, is `result = false`, php's own `else`.
    ///
    /// ```php
    /// class Test {
    ///     public function __isset($x) { $GLOBALS["obj"] = 24; return true; }
    ///     public function __get($x) { var_dump($this); return 42; }
    /// }
    /// $obj = new Test;
    /// $name = "foo";
    /// var_dump(empty($obj->$name));
    /// var_dump($obj);
    /// ```
    /// ```expect
    /// object(Test)#1 (0) {
    /// }
    /// bool(false)
    /// int(24)
    /// ```
    fn isset_then_truth(
        &mut self,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
        issetter: Function,
    ) -> Result<bool, Thrown> {
        let answered = self.serve_accessor(object, issetter, name, &[])?;
        let truthy = crate::api::coerce::truthy(self, &answered);
        self.release(answered);
        let truthy = truthy?;
        if test != PropertyTest::NotEmpty || !truthy {
            return Ok(truthy);
        }
        let Some(getter) = self
            .magic_accessor(object.class(), b"__get")
            .filter(|_| self.guard_word(object, name) & IN_GET == 0)
        else {
            return Ok(false);
        };
        let answered = self.call_accessor(object, getter, IN_GET, name, &[])?;
        let truthy = crate::api::coerce::truthy(self, &answered);
        self.release(answered);
        truthy
    }

    /// `zend_std_has_property`'s `lazy_init:` arm: the object
    /// initialised and asked again on what that answers, under
    /// `IN_ISSET` there where its class has a `__isset`, so the accessor
    /// is not served twice for one question.
    ///
    /// ```php
    /// class C { public $a = 1; }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function ($o) { echo "init o\n"; });
    /// var_dump(isset($o->nope));
    /// $p = $r->newLazyProxy(function () { echo "factory\n"; return new C; });
    /// unset($p->a);
    /// var_dump($p);
    /// ```
    /// ```expect
    /// init o
    /// bool(false)
    /// factory
    /// lazy proxy object(C)#4 (1) {
    ///   ["instance"]=>
    ///   object(C)#5 (0) {
    ///   }
    /// }
    /// ```
    fn lazy_has(
        &mut self,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        let instance = self.lazy_init(object)?;
        if self.serves_accessor(instance.class(), b"__isset")
            && self.guard_word(instance, name) & IN_ISSET == 0
        {
            self.guard_take(instance, name, IN_ISSET);
            let held = self.has_property(instance, name, test);
            self.guard_drop(instance, name, IN_ISSET);
            return held;
        }
        self.has_property(instance, name, test)
    }

    /// `zend_std_unset_property`: a declared slot the scope may reach is
    /// left uninitialised, what it held released; one it may not reach
    /// is php's `Error`, and so is one whose set visibility the scope
    /// may not write, whose word here is `unset`
    /// (`Z/Zend/zend_object_handlers.c:1583`); a dynamic name is
    /// removed from the table, separated first where a cast shares it.
    ///
    /// A class with a `__unset` is served where the placement found
    /// nothing to remove: a name no class declared that the table does
    /// not hold, a declared untyped slot an `unset` already emptied,
    /// and, since the placement is made silent by the accessor
    /// (`zend_get_property_offset(ce, name, ce->__unset != NULL, ...)`),
    /// a declared slot the scope may not reach. It is served under
    /// `IN_UNSET`, so an `__unset` unsetting the name it was called for
    /// removes nothing rather than reaching itself, and the `Error` for
    /// the property the scope may not reach is raised on that second
    /// pass (`zend_wrong_offset`).
    ///
    /// ```php
    /// class M {
    ///     public $kept = 1;
    ///     public function __unset($k) { echo "unset $k\n"; }
    /// }
    /// $m = new M;
    /// $m->dyn = 2;
    /// unset($m->kept);
    /// unset($m->dyn);
    /// unset($m->nope);
    /// var_dump($m);
    /// class U { public function __unset($k) { echo "unset $k\n"; unset($this->w); } }
    /// $u = new U;
    /// unset($u->w);
    /// ```
    /// ```expect
    ///
    /// Deprecated: Creation of dynamic property M::$dyn is deprecated in %s on line 7
    /// unset nope
    /// object(M)#1 (0) {
    /// }
    /// unset w
    /// ```
    ///
    /// `IS_PROP_UNINIT` tells a typed slot nothing has written, which
    /// passes the magic methods by, from one an `unset` emptied, which
    /// reaches them; the set visibility is checked where php checks it,
    /// inside the accessor, and names the class that declared the
    /// property:
    ///
    /// ```php
    /// class C { public private(set) int $a = 1; public int $u;
    ///     function __construct() { unset($this->a); } }
    /// class D extends C {
    ///     function __unset($n) { echo "__unset $n\n"; unset($this->a); }
    ///     function __get($n) { echo "__get $n\n"; return 7; }
    /// }
    /// $d = new D;
    /// try { unset($d->a); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($d->a);
    /// try { var_dump($d->u); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// unset($d->u);
    /// var_dump($d->u);
    /// ```
    /// ```expect
    /// __unset a
    /// Cannot unset private(set) property C::$a from scope D
    /// __get a
    /// int(7)
    /// Typed property C::$u must not be accessed before initialization
    /// __get u
    /// int(7)
    /// ```
    /// `zend_std_unset_property`'s declared arm over a slot that holds
    /// a value: the value given up with the slot left `UNDEF`, no longer
    /// `IS_PROP_UNINIT` whatever it was made with, and then
    /// `HT_FLAGS(zobj->properties) |= HASH_FLAG_HAS_EMPTY_IND`, so a
    /// destructor the release served counted the slot still
    /// ([`Request::property_count`]).
    fn empty_slot(&mut self, object: PhpObject, slot: u32) {
        let o = object.heap();
        if let Some(cell) = o.slot(slot).as_reference() {
            self.slot_source_gone(object, slot, cell);
        }
        Heap::object_set_slot(&mut Hooks(self), o, slot, Zval::UNDEF);
        o.set_slot_bit(slot, php_heap::PropBit::Uninit, false);
        if let Some(table) = o.properties() {
            table.set_has_empty_ind(true);
        }
    }

    pub(crate) fn unset_property(&mut self, object: PhpObject, name: &[u8]) -> Result<(), Thrown> {
        let class = object.class();
        let o = object.heap();
        if let Some(hooked) = self.hooked(class, name)
            && self.hooked_accessible(&hooked)
        {
            return Err(self.unset_hooked(object, name));
        }
        let unsetter = self.magic_accessor(class, b"__unset");
        let mut wrong = false;
        if let Some(placed) = self.declared_slot(class, name) {
            if self.reaches(&placed) {
                let empty = o.slot(placed.slot).is_undef();
                // `error = Z_TYPE_P(slot) != IS_UNDEF || (Z_PROP_FLAG_P(slot)
                // & IS_PROP_UNINIT) || !zobj->ce->__unset`: a slot that
                // holds a value, and a typed one nothing has written,
                // are held to the set visibility here; an untyped slot
                // an `unset` emptied is the `__unset`'s.
                let waits = self.lazy_slot(o, placed.slot);
                let uninit = o.slot_uninit(placed.slot);
                let in_unset = unsetter.is_some() && self.guard_word(object, name) & IN_UNSET != 0;
                if !empty || uninit || unsetter.is_none() || in_unset || waits {
                    // `zend_readonly_property_unset_error`, ahead of the
                    // set visibility: an initialised readonly property is
                    // unset from no scope, the class's own included, save
                    // inside `__clone`, where the slot is
                    // `IS_PROP_REINITABLE`.
                    if placed.readonly
                        && !empty
                        && !o.slot_bit(placed.slot, php_heap::PropBit::Reinitable)
                    {
                        return Err(self.readonly_modification(placed.declaring, name, "unset"));
                    }
                    if let Some(thrown) = self.set_refusal(&placed, name, "unset") {
                        return Err(thrown);
                    }
                    if !empty {
                        self.empty_slot(object, placed.slot);
                    } else if uninit && !waits {
                        // `Z_PROP_FLAG_P(slot) = 0`: the bit reset and
                        // `__unset` passed by.
                        o.set_slot_bit(placed.slot, php_heap::PropBit::Uninit, false);
                    } else if waits {
                        // `IS_PROP_LAZY`: the object initialised and the
                        // property unset on what that answers.
                        let instance = self.lazy_init(object)?;
                        return self.unset_property(instance, name);
                    }
                    return Ok(());
                }
            } else {
                if unsetter.is_none() {
                    return Err(self.inaccessible_property(class, placed.visibility, name));
                }
                wrong = true;
            }
        } else if let Some(miss) = self.unplaced(class, name, unsetter.is_some()) {
            if unsetter.is_none() {
                return Err(self.unplaced_error(class, name, miss));
            }
            wrong = true;
        } else if let Some(table) = o.properties() {
            let name = self.declared_string(name)?;
            let key = php_heap::Key::Str(name.heap());
            if table.contains(key) {
                let deleted = self
                    .separated_properties(o)
                    .map(|table| self.array_del(table, key));
                name.give_up(self);
                return deleted.map(|_| ());
            }
            name.give_up(self);
        }
        // GH-21478: a proxy that forwards, unset from inside the
        // instance's own `__unset` for this name, unsets the instance's.
        if let Some(instance) = self.lazy_instance(o)
            && self.guard_word(instance, name) & IN_UNSET != 0
        {
            return self.unset_property(instance, name);
        }
        if let Some(unsetter) = unsetter {
            if self.guard_word(object, name) & IN_UNSET == 0 {
                let answered = self.call_accessor(object, unsetter, IN_UNSET, name, &[])?;
                self.release(answered);
                return Ok(());
            }
            if wrong && let Some(thrown) = self.wrong_offset(class, name) {
                return Err(thrown);
            }
        }
        // A lazy object with nothing that answered the name is
        // initialised and the name unset on what that answers, under
        // `IN_UNSET` there where the `__unset` guard stood here.
        if o.laziness() != php_heap::Laziness::Eager {
            let instance = self.lazy_init(object)?;
            if unsetter.is_some() && self.guard_word(instance, name) & IN_UNSET == 0 {
                self.guard_take(instance, name, IN_UNSET);
                let unset = self.unset_property(instance, name);
                self.guard_drop(instance, name, IN_UNSET);
                return unset;
            }
            return self.unset_property(instance, name);
        }
        Ok(())
    }
}
