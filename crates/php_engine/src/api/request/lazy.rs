//! Lazy objects (design §5.4), `Z/Zend/zend_lazy_objects.c` at the tag:
//! a ghost, initialised in place by its initializer the first time a
//! property's backing store is touched, and a proxy, whose factory
//! returns the instance every property access is forwarded to from then
//! on. What an object is, is its [`Laziness`], the two bits php keeps in
//! `OBJ_EXTRA_FLAGS` read as one state; what goes with that state, the
//! initializer while the object waits and the instance once a proxy's
//! factory ran, is a row of the request's [`Table`] keyed by the
//! object's id, php's `EG(lazy_objects_store)` of `zend_lazy_object_info`
//! (ADR-0005: request-scoped). Which declared slots are still lazy,
//! php's `IS_PROP_LAZY` beside each slot, is part of that row.
//!
//! Every trigger reaches one step, [`Request::lazy_init`], php's
//! `zend_lazy_object_init`: the standard handlers call it where php's
//! `zend_std_*` do, at the arm a slot nothing has written or a name no
//! table holds falls to, and it answers the object whose properties the
//! access goes on with, the ghost itself or the proxy's instance.

use php_heap::{Heap, Laziness, Obj};
use php_unit::decl::Bare;

use super::{Hooks, IN_GET, Request};
use crate::api::abi::Callable;
use crate::api::call::Target;
use crate::api::raising::{Thrown, throw_error};
use crate::api::values::{ClassId, Owner, PhpArray, PhpObject, TypeTag, Value};

/// `ZEND_LAZY_OBJECT_SKIP_INITIALIZATION_ON_SERIALIZE`.
pub(crate) const SKIP_INITIALIZATION_ON_SERIALIZE: i64 = 1 << 3;
/// `ZEND_LAZY_OBJECT_SKIP_DESTRUCTOR`.
pub(crate) const SKIP_DESTRUCTOR: i64 = 1 << 4;

/// The strategy `zend_object_make_lazy` is asked for,
/// `ZEND_LAZY_OBJECT_STRATEGY_GHOST` or `ZEND_LAZY_OBJECT_STRATEGY_PROXY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Strategy {
    Ghost,
    Proxy,
}

/// `zend_lazy_object_info.u`: the initializer and the slots still lazy
/// while nothing has initialised the object, and the instance once a
/// proxy's factory returned one.
#[derive(Debug)]
enum State {
    Waiting {
        initializer: Owner<Callable>,
        lazy: Vec<bool>,
    },
    Instance(PhpObject),
}

#[derive(Debug)]
struct Info {
    state: State,
    /// `ZEND_LAZY_OBJECT_SKIP_INITIALIZATION_ON_SERIALIZE`.
    skip_on_serialize: bool,
}

/// `EG(lazy_objects_store)`: every lazy object's id against what goes
/// with its state. A row outlives the object's two bits only while its
/// initializer runs, which is how `zend_lazy_object_has_stale_info`
/// tells an object being initialised from one that never was lazy.
#[derive(Debug, Default)]
pub(crate) struct Table {
    rows: std::collections::HashMap<u32, Info>,
}

/// The `IN_GET` [`Request::lazy_place`] set on an instance's guard for
/// `name` while a place is resolved there on behalf of a proxy whose
/// own `__get` guard held the name.
#[must_use = "the guard is given back once the place is resolved"]
pub(crate) struct LentGuard {
    instance: PhpObject,
    name: Vec<u8>,
}

impl Request {
    fn lazy_row(&self, o: Obj) -> Option<&Info> {
        self.lazy.rows.get(&o.id().0)
    }

    /// `Z_PROP_FLAG_P(slot) & IS_PROP_LAZY`: whether slot `slot` of the
    /// object still waits for the initializer. Every slot of a proxy
    /// that forwards is lazy, since each access goes to the instance.
    pub(crate) fn lazy_slot(&self, o: Obj, slot: u32) -> bool {
        match o.laziness() {
            Laziness::Eager => false,
            Laziness::Forwarding => true,
            Laziness::Ghost | Laziness::Proxy => matches!(
                self.lazy_row(o),
                Some(Info { state: State::Waiting { lazy, .. }, .. })
                    if lazy.get(slot as usize).copied().unwrap_or(false)
            ),
        }
    }

    /// `zend_lazy_object_get_instance` over a proxy that forwards: the
    /// instance its factory returned; `None` for any other object.
    pub(crate) fn lazy_instance(&self, o: Obj) -> Option<PhpObject> {
        if o.laziness() != Laziness::Forwarding {
            return None;
        }
        match self.lazy_row(o) {
            Some(Info {
                state: State::Instance(instance),
                ..
            }) => Some(*instance),
            _ => None,
        }
    }

    /// `zend_lazy_object_get_initializer_zv`: the initializer as it was
    /// given, a view, while the object waits for it.
    pub(crate) fn lazy_initializer(&self, o: Obj) -> Option<Value> {
        if !matches!(o.laziness(), Laziness::Ghost | Laziness::Proxy) {
            return None;
        }
        match self.lazy_row(o) {
            Some(Info {
                state: State::Waiting { initializer, .. },
                ..
            }) => Some(initializer.value()),
            _ => None,
        }
    }

    /// `zend_lazy_object_initialize_on_serialize`'s answer turned round:
    /// whether serializing the object reads its properties as they
    /// stand, `SKIP_INITIALIZATION_ON_SERIALIZE`.
    pub(crate) fn lazy_skips_serialize(&self, o: Obj) -> bool {
        o.laziness() != Laziness::Eager && self.lazy_row(o).is_some_and(|row| row.skip_on_serialize)
    }

    /// The object whose table a reader that must not initialise reads,
    /// `zend_get_properties_no_lazy_init`'s forward: a proxy's instance
    /// once its factory ran, and the object itself otherwise.
    pub(crate) fn lazy_reader(&self, object: PhpObject) -> PhpObject {
        let mut at = object;
        for _ in 0..64 {
            match self.lazy_instance(at.heap()) {
                Some(instance) => at = instance,
                None => break,
            }
        }
        at
    }

    /// `zend_get_properties_no_lazy_init`'s `rebuild_object_properties_internal`
    /// (`Z/Zend/zend_object_handlers.c:151-163`): the table of the object
    /// a reader of `object` reads, the one the initialised proxies lead
    /// to, laid down where it has none, nothing initialised. php's debug
    /// walk leaves a ghost's table standing so, and the ghost's
    /// initializer takes it as its snapshot.
    pub(crate) fn lay_properties_no_lazy_init(
        &mut self,
        object: PhpObject,
    ) -> Result<PhpObject, Thrown> {
        let reader = self.lazy_reader(object);
        let built = self.heap.object_properties(reader.heap());
        self.fatal_of(built)?;
        Ok(reader)
    }

    /// The values a lazy object's row owns, for the cycle collector,
    /// `zend_lazy_object_get_gc` less the slots, which the heap walks:
    /// the instance of a proxy that forwards, and otherwise the
    /// initializer's receiver and its value.
    pub(crate) fn lazy_gc(&self, o: Obj, visit: &mut dyn FnMut(Value)) {
        match self.lazy_row(o).map(|row| &row.state) {
            Some(State::Instance(instance)) => visit(instance.as_value()),
            Some(State::Waiting { initializer, .. }) => {
                initializer.visit(visit);
                // The closure the row owns apart, beside the value.
                if let Target::Closure(closure) = initializer.target() {
                    visit(closure.as_value());
                }
            }
            None => {}
        }
    }

    /// `zend_lazy_object_del_info`: the row given up, the initializer
    /// or the instance with it.
    ///
    /// ```php
    /// class C { public $a = 1; public function __destruct() { echo "destruct\n"; } }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function () { echo "init\n"; });
    /// $o = null;
    /// echo "ghost freed\n";
    /// $p = $r->newLazyProxy(function () { echo "factory\n"; return new C; });
    /// $p->a;
    /// $p = null;
    /// echo "end\n";
    /// ```
    /// ```expect
    /// ghost freed
    /// factory
    /// destruct
    /// end
    /// ```
    pub(crate) fn lazy_forget(&mut self, o: Obj) {
        let Some(row) = self.lazy.rows.remove(&o.id().0) else {
            return;
        };
        match row.state {
            State::Waiting { initializer, .. } => self.release_initializer(initializer),
            State::Instance(instance) => self.release(instance.as_value()),
        }
    }

    /// `zend_lazy_object_info_dtor_func`'s initializer arm: the value
    /// given up, `zval_ptr_dtor(&info->u.initializer.zv)`, and then the
    /// callable's own owners, `zend_fcc_dtor`, the closure last. A
    /// closure the row alone held is a possible root at the first
    /// release and freed at the second, which is when php's collector
    /// may start: here it starts while the ghost the last property's write
    /// realised is no longer lazy, so the destructor it serves may make
    /// it lazy again.
    ///
    /// ```php
    /// class C { public $a; }
    /// class D { public $self; function __construct() { $this->self = $this; }
    ///     function __destruct() { global $obj, $r; $r->resetAsLazyGhost($obj, function () {}); } }
    /// new D();
    /// $r = new ReflectionClass(C::class);
    /// for ($i = 0; $i < 10000; $i++) {
    ///     $obj = $r->newLazyGhost(function ($obj) {});
    ///     $obj2 = $obj; unset($obj2);
    ///     $r->getProperty('a')->setRawValueWithoutLazyInitialization($obj, $obj);
    /// }
    /// var_dump($r->isUninitializedLazyObject($obj), gc_status()['runs']);
    /// ```
    /// ```expect
    /// bool(false)
    /// int(2)
    /// ```
    fn release_initializer(&mut self, initializer: Owner<Callable>) {
        let closure = match initializer.target() {
            Target::Closure(closure) => Some(closure),
            _ => None,
        };
        initializer.give_up(self);
        if let Some(closure) = closure {
            self.release(closure.as_value());
        }
    }

    /// `zend_class_can_be_lazy`, with the `Error` `zend_object_make_lazy`
    /// raises for a class that cannot: an internal class other than
    /// `stdClass`, or one whose chain has such a class above it.
    pub(crate) fn lazy_refusal(&mut self, class: ClassId) -> Option<Thrown> {
        let std_class = self.class_named(Bare::resolved(b"stdClass"));
        let internal =
            |req: &Request, at: ClassId| req.class_is_internal(at) && Some(at) != std_class;
        let name = self.symbols().class_name(class);
        if internal(self, class) {
            return Some(throw_error(
                self,
                "Error",
                &format!("Cannot make instance of internal class lazy: {name} is internal"),
            ));
        }
        let parent = self
            .ancestors(class)
            .find(|&parent| internal(self, parent))?;
        let parent = self.symbols().class_name(parent);
        Some(throw_error(
            self,
            "Error",
            &format!(
                "Cannot make instance of internal class lazy: {name} inherits internal class {parent}"
            ),
        ))
    }

    /// `zend_object_make_lazy` (`Z/Zend/zend_lazy_objects.c:225`): a new
    /// object of `class` made lazy where `object` is none, and otherwise
    /// `object` reset and made lazy, its destructor served first unless
    /// `SKIP_DESTRUCTOR`, its dynamic properties removed and each declared
    /// slot of `class` left waiting, a `readonly` one already written on
    /// a final class or as a final property excepted. An object with no
    /// slot left waiting is not lazy at all. The initializer is kept with
    /// one more owner.
    ///
    /// ```php
    /// class E {}
    /// class C { public $a = 1; }
    /// $r = new ReflectionClass('E');
    /// var_dump($r->isUninitializedLazyObject($r->newLazyGhost(fn () => null)));
    /// $c = new ReflectionClass('C');
    /// $p = $c->newLazyProxy(function () { echo "factory\n"; return new C; });
    /// $p->a;
    /// $c->resetAsLazyGhost($p, function ($o) { echo "init\n"; $o->a = 3; });
    /// var_dump($p);
    /// var_dump($p->a);
    /// ```
    /// ```expect
    /// bool(false)
    /// factory
    /// lazy ghost object(C)#4 (0) {
    /// }
    /// init
    /// int(3)
    /// ```
    pub(crate) fn make_lazy(
        &mut self,
        object: Option<PhpObject>,
        class: ClassId,
        initializer: &Callable,
        strategy: Strategy,
        options: i64,
    ) -> Result<PhpObject, Thrown> {
        if let Some(thrown) = self.lazy_refusal(class) {
            return Err(thrown);
        }
        let (object, lazy) = match object {
            None => {
                let object = crate::api::call::new_object_made(self, class)?;
                let o = object.heap();
                // `Z_PROP_FLAG_P(p) = IS_PROP_UNINIT | IS_PROP_LAZY` over
                // every slot, as `zend_object_make_lazy` lays them.
                for i in 0..o.slots() {
                    Heap::object_set_slot(&mut Hooks(self), o, i, php_heap::Zval::UNDEF);
                    o.set_slot_bit(i, php_heap::PropBit::Uninit, true);
                }
                (object, vec![true; o.slots() as usize])
            }
            Some(object) => (object, self.lazy_reset(object, class, options)?),
        };
        let o = object.heap();
        if !lazy.contains(&true) {
            return Ok(object);
        }
        o.set_laziness(match strategy {
            Strategy::Ghost => Laziness::Ghost,
            Strategy::Proxy => Laziness::Proxy,
        });
        // `ZVAL_COPY(&info->u.initializer.zv, ...)` and `zend_fcc_dup`:
        // the row owns the value and, apart, the closure `fcc.closure`
        // names, so a closure initializer has two owners here.
        let held = Owner::take(self, initializer.clone());
        if let Target::Closure(closure) = initializer.target() {
            self.addref(&closure.as_value());
        }
        self.lazy.rows.insert(
            o.id().0,
            Info {
                state: State::Waiting {
                    initializer: held,
                    lazy,
                },
                skip_on_serialize: options & SKIP_INITIALIZATION_ON_SERIALIZE != 0,
            },
        );
        Ok(object)
    }

    /// The reset half of `zend_object_make_lazy`: the slots of `class`
    /// that are left waiting, each marked in the answer.
    fn lazy_reset(
        &mut self,
        object: PhpObject,
        class: ClassId,
        options: i64,
    ) -> Result<Vec<bool>, Thrown> {
        let o = object.heap();
        let iterating = |at: Obj| at.properties().is_some_and(php_heap::Arr::has_iterators);
        let forwarded = self.lazy_instance(o);
        if iterating(o) || forwarded.is_some_and(|i| iterating(i.heap())) {
            return Err(throw_error(
                self,
                "Error",
                "Can not reset an object during property iteration",
            ));
        }
        if o.laziness() == Laziness::Forwarding {
            o.set_laziness(Laziness::Eager);
            self.lazy_forget(o);
        } else {
            if self.lazy_row(o).is_some() {
                return Err(throw_error(
                    self,
                    "Error",
                    "Can not reset an object while it is being initialized",
                ));
            }
            if options & SKIP_DESTRUCTOR == 0
                && !o.destructor_called()
                && self.has_destructor(object)
            {
                o.set_destructor_called(true);
                let held = Owner::take(self, object);
                let table = self.object_handlers(o);
                table.dtor_obj().serve(self, object);
                held.give_up(self);
                if let Some(pending) = self.take_pending_view() {
                    return Err(Thrown::of(Some(pending)));
                }
            }
        }
        o.set_destructor_called(false);
        if let Some(table) = self.heap.object_take_properties(o) {
            self.release(Value::array(PhpArray::of(table)));
        }
        let placed = self.placed_properties(object.class()).to_vec();
        let reset = self.class_slots(class) as usize;
        let final_class = self
            .symbols()
            .class_decl(object.class())
            .is_some_and(|decl| decl.flags.is_final);
        let mut lazy = vec![false; o.slots() as usize];
        for (i, &info) in placed.iter().enumerate().take(reset) {
            let slot = u32::try_from(i).unwrap_or(u32::MAX);
            if !o.slot(slot).is_undef() {
                let kept = self
                    .property_decl(info)
                    .is_some_and(|decl| decl.is_readonly && (final_class || decl.is_final));
                if kept {
                    continue;
                }
                self.dtor_property(object, slot);
            }
            o.set_slot_bit(slot, php_heap::PropBit::Uninit, true);
            if let Some(at) = lazy.get_mut(i) {
                *at = true;
            }
        }
        Ok(lazy)
    }

    /// `zend_lazy_object_init` (`Z/Zend/zend_lazy_objects.c:560`), the
    /// one step every trigger takes: the object an access goes on with.
    /// A ghost is initialised in place and is the answer; a proxy's
    /// factory is served and its instance is the answer; a proxy that
    /// forwards answers its instance, initialised first if it was made
    /// lazy since. An object that is not lazy is the answer as it stands.
    ///
    /// ```php
    /// class C { public $a = 1; }
    /// class D extends C { public $b; }
    /// $r = new ReflectionClass('C');
    /// foreach ([fn () => 1, fn () => new stdClass, fn ($p) => $p, fn () => $r->newLazyGhost(fn () => null)] as $f) {
    ///     $p = $r->newLazyProxy($f);
    ///     try { $p->a; } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
    /// }
    /// $d = (new ReflectionClass('D'))->newLazyProxy(fn () => new C);
    /// try { $d->a; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($r->isUninitializedLazyObject($p));
    /// ```
    /// ```expect
    /// TypeError: Lazy proxy factory must return an instance of a class compatible with C, int returned
    /// TypeError: The real instance class stdClass is not compatible with the proxy class C. The proxy must be a instance of the same class as the real instance, or a sub-class with no additional properties, and no overrides of the __destructor or __clone methods.
    /// Error: Lazy proxy factory must return a non-lazy object
    /// Error: Lazy proxy factory must return a non-lazy object
    /// The real instance class C is not compatible with the proxy class D. The proxy must be a instance of the same class as the real instance, or a sub-class with no additional properties, and no overrides of the __destructor or __clone methods.
    /// bool(true)
    /// ```
    pub(crate) fn lazy_init(&mut self, object: PhpObject) -> Result<PhpObject, Thrown> {
        match object.heap().laziness() {
            Laziness::Eager => Ok(object),
            Laziness::Forwarding => match self.lazy_instance(object.heap()) {
                Some(instance) if instance.heap().laziness() != Laziness::Eager => {
                    self.lazy_init(instance)
                }
                Some(instance) => Ok(instance),
                None => Ok(object),
            },
            Laziness::Ghost => self.lazy_init_ghost(object),
            Laziness::Proxy => self.lazy_init_proxy(object),
        }
    }

    /// `zend_std_get_property_ptr_ptr`'s two lazy arms, for a site that
    /// writes in place (`FETCH_OBJ_W`, a compound assignment, an
    /// increment): the object the site places `name` on. A declared slot
    /// the scope may reach that waits, and a name the object holds
    /// nowhere that no `__get` serves, initialise the object and are
    /// placed on what that answers; anything else is placed on the
    /// object as it stands.
    ///
    /// ```php
    /// class C { public $a = 1; }
    /// $r = new ReflectionClass('C');
    /// $q = $r->newLazyGhost(function ($o) { echo "init q\n"; });
    /// $q->a = 5;
    /// var_dump($q);
    /// $s = $r->newLazyProxy(function () { echo "factory\n"; return new C; });
    /// $s->a .= 'x';
    /// $s->a++;
    /// var_dump($s->a);
    /// ```
    /// ```expect
    /// init q
    /// object(C)#3 (1) {
    ///   ["a"]=>
    ///   int(5)
    /// }
    /// factory
    ///
    /// Deprecated: Increment on non-numeric string is deprecated, use str_increment() instead in %s on line 9
    /// string(2) "1y"
    /// ```
    ///
    /// Where the object's `__get` guard holds `name` (the place is asked
    /// from inside that `__get`), the guard is lent to what the
    /// initialisation answers for as long as the place is resolved there
    /// (GH-20873, GH-20875): php sets `IN_GET` on the instance's guard
    /// around its `get_property_ptr_ptr` (`Z/Zend/zend_object_handlers.c:1444-1458`,
    /// `:1504-1518`), so the instance lays the property down rather than
    /// serve its own `__get` a second time. The [`LentGuard`] is the
    /// caller's to give back ([`Request::give_back_guard`]).
    ///
    /// ```php
    /// class A { public $_;
    ///     public function __get($n) { echo "get $n\n"; global $obj; $obj->x =& $this->_; } }
    /// $rc = new ReflectionClass(A::class);
    /// $obj = $rc->newLazyProxy(fn () => new A);
    /// $rc->initializeLazyObject($obj);
    /// try { $obj->p; } catch (Error $e) { echo $e->getMessage(), "\n", $e->getTraceAsString(), "\n"; }
    /// ```
    /// ```expect
    /// get p
    /// get x
    ///
    /// Deprecated: Creation of dynamic property A::$x is deprecated in %s on line 3
    ///
    /// Notice: Indirect modification of overloaded property A::$x has no effect in %s on line 3
    /// Cannot assign by reference to overloaded object
    /// #0 %s(7): A->__get('p')
    /// #1 {main}
    /// ```
    pub(crate) fn lazy_place(
        &mut self,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(PhpObject, Option<LentGuard>), Thrown> {
        let o = object.heap();
        if o.laziness() == Laziness::Eager {
            return Ok((object, None));
        }
        let initialises = match self.declared_slot(object.class(), name) {
            Some(placed) if self.reaches(&placed) => {
                o.slot(placed.slot).is_undef() && self.lazy_slot(o, placed.slot)
            }
            Some(_) => false,
            None => Self::dynamic_prop(o, name).is_none() && !self.overloaded_place(object, name),
        };
        if !initialises {
            return Ok((object, None));
        }
        let guarded = self.serves_accessor(object.class(), b"__get")
            && self.guard_word(object, name) & IN_GET != 0;
        let instance = self.lazy_init(object)?;
        if guarded
            && self.uses_guards(instance.class())
            && self.guard_word(instance, name) & IN_GET == 0
        {
            self.guard_take(instance, name, IN_GET);
            let lent = LentGuard {
                instance,
                name: name.to_vec(),
            };
            return Ok((instance, Some(lent)));
        }
        Ok((instance, None))
    }

    /// The `IN_GET` a [`LentGuard`] took on the instance, given back once
    /// the place is resolved.
    pub(crate) fn give_back_guard(&mut self, lent: Option<LentGuard>) {
        if let Some(LentGuard { instance, name }) = lent {
            self.guard_drop(instance, &name, IN_GET);
        }
    }

    /// The initializer and the waiting slots of an object that waits.
    fn lazy_waiting(&self, o: Obj) -> Option<(Callable, Vec<bool>)> {
        match self.lazy_row(o) {
            Some(Info {
                state: State::Waiting { initializer, lazy },
                ..
            }) => Some(((**initializer).clone(), lazy.clone())),
            _ => None,
        }
    }

    /// The ghost arm of `zend_lazy_object_init`: the object held one
    /// more owner and its bits cleared, so the initializer's own accesses
    /// initialise nothing; the slots and the dynamic table snapshotted
    /// and each waiting slot given its default; the initializer served
    /// with the object; and on a throw, or on a return that is not
    /// `null`, every slot and the table put back as they were and the
    /// object left a ghost.
    ///
    /// ```php
    /// class C { public $a = 1; }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function ($o) { var_dump($o->a); $o->a = 2; return 1; });
    /// try { $o->a; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($r->isUninitializedLazyObject($o), $o);
    /// ```
    /// ```expect
    /// int(1)
    /// Lazy object initializer must return NULL or no value
    /// bool(true)
    /// lazy ghost object(C)#3 (0) {
    /// }
    /// ```
    fn lazy_init_ghost(&mut self, object: PhpObject) -> Result<PhpObject, Thrown> {
        let o = object.heap();
        let Some((initializer, lazy)) = self.lazy_waiting(o) else {
            return Ok(object);
        };
        let defaults = self.property_defaults(object.class())?;
        self.addref(&object.as_value());
        o.set_laziness(Laziness::Eager);
        let table = o.properties();
        if let Some(table) = table {
            table.addref();
        }
        let mut snapshot = Vec::with_capacity(o.slots() as usize);
        for (i, default) in (0..o.slots()).zip(defaults) {
            let held = o.slot(i);
            held.addref();
            snapshot.push(held);
            let waiting = lazy.get(i as usize).copied().unwrap_or(false);
            match (waiting, default) {
                (true, Some(value)) => {
                    if let Some(place) = o.slot_ref(i) {
                        place.write(value.into_zval());
                    }
                }
                (false, Some(value)) => self.release(value),
                (_, None) => {}
            }
        }
        let served = crate::api::call::call_made(self, &initializer, &[object.as_value()]);
        let outcome = match served {
            Err(thrown) => {
                self.lazy_revert(o, snapshot, table);
                Err(thrown)
            }
            Ok(returned) if returned.tag() != TypeTag::Null => {
                self.lazy_revert(o, snapshot, table);
                self.release(returned);
                Err(throw_error(
                    self,
                    "TypeError",
                    "Lazy object initializer must return NULL or no value",
                ))
            }
            Ok(_) => {
                for held in snapshot {
                    self.release(Value::of_slot(held));
                }
                if let Some(table) = table {
                    self.release(Value::array(PhpArray::of(table)));
                }
                self.lazy_forget(o);
                Ok(object)
            }
        };
        self.lazy_settle(object, outcome)
    }

    /// `zend_lazy_object_revert_init`: every slot back to what the
    /// snapshot holds and the dynamic table back to the one the object
    /// had, what the initializer left released; the object a ghost again.
    fn lazy_revert(&mut self, o: Obj, snapshot: Vec<php_heap::Zval>, table: Option<php_heap::Arr>) {
        let object = PhpObject::of(o);
        for (i, held) in (0..o.slots()).zip(snapshot) {
            self.dtor_property(object, i);
            Heap::object_set_slot(&mut Hooks(self), o, i, held);
            // `ZEND_REF_ADD_TYPE_SOURCE` over a cell the snapshot put
            // back: the property types it again.
            if let Some(cell) = held.as_reference() {
                self.slot_source_added(object, i, cell);
            }
        }
        let now = self.heap.object_take_properties(o);
        match (table, now) {
            (Some(table), Some(now)) if table.is_same(now) => {
                self.heap.object_set_properties(o, table);
                self.release(Value::array(PhpArray::of(table)));
            }
            (table, now) => {
                if let Some(now) = now {
                    self.release(Value::array(PhpArray::of(now)));
                }
                if let Some(table) = table {
                    self.heap.object_set_properties(o, table);
                }
            }
        }
        o.set_laziness(Laziness::Ghost);
    }

    /// The proxy arm of `zend_lazy_object_init`
    /// (`zend_lazy_object_init_proxy`): the factory served with the proxy
    /// and its answer held to php's three refusals, then kept as the
    /// instance, the proxy's own properties released so every access
    /// from here on is forwarded.
    fn lazy_init_proxy(&mut self, object: PhpObject) -> Result<PhpObject, Thrown> {
        let o = object.heap();
        let Some((initializer, _)) = self.lazy_waiting(o) else {
            return Ok(object);
        };
        self.addref(&object.as_value());
        o.set_laziness(Laziness::Eager);
        let served = crate::api::call::call_made(self, &initializer, &[object.as_value()]);
        let outcome = match served {
            Err(thrown) => {
                o.set_laziness(Laziness::Proxy);
                Err(thrown)
            }
            Ok(returned) => self.lazy_take_instance(object, returned),
        };
        self.lazy_settle(object, outcome)
    }

    /// What a proxy's factory returned, held to php's refusals and
    /// kept as the instance.
    fn lazy_take_instance(
        &mut self,
        object: PhpObject,
        returned: Value,
    ) -> Result<PhpObject, Thrown> {
        let o = object.heap();
        let class = self.symbols().class_name(object.class());
        let Some(instance) = returned.as_object() else {
            o.set_laziness(Laziness::Proxy);
            let shown = crate::ops::value_name(self, &returned);
            self.release(returned);
            return Err(throw_error(
                self,
                "TypeError",
                &format!(
                    "Lazy proxy factory must return an instance of a class compatible with {class}, {shown} returned"
                ),
            ));
        };
        if !self.lazy_compatible(instance.class(), object.class()) {
            o.set_laziness(Laziness::Proxy);
            let shown = self.symbols().class_name(instance.class());
            self.release(returned);
            return Err(throw_error(
                self,
                "TypeError",
                &format!(
                    "The real instance class {shown} is not compatible with the proxy class {class}. The proxy must be a instance of the same class as the real instance, or a sub-class with no additional properties, and no overrides of the __destructor or __clone methods."
                ),
            ));
        }
        if instance == object || instance.heap().laziness() != Laziness::Eager {
            o.set_laziness(Laziness::Proxy);
            self.release(returned);
            return Err(throw_error(
                self,
                "Error",
                "Lazy proxy factory must return a non-lazy object",
            ));
        }
        let Some(row) = self.lazy.rows.get_mut(&o.id().0) else {
            self.release(returned);
            return Ok(object);
        };
        let waiting = std::mem::replace(&mut row.state, State::Instance(instance));
        if let State::Waiting { initializer, .. } = waiting {
            self.release_initializer(initializer);
        }
        o.set_laziness(Laziness::Forwarding);
        if let Some(table) = self.heap.object_take_properties(o) {
            self.release(Value::array(PhpArray::of(table)));
        }
        // "unset() properties of the proxy", each as
        // `zend_object_dtor_property` gives one up, so a cell bound to a
        // slot of the proxy is typed by it no longer: the instance's
        // slots are the ones that stand now.
        for i in 0..instance.heap().slots().min(o.slots()) {
            self.dtor_property(object, i);
        }
        Ok(instance)
    }

    /// `zend_lazy_object_compatible`: the instance's class is the
    /// proxy's, or a parent of it that has as many properties and the
    /// same `__destruct` and `__clone`.
    fn lazy_compatible(&self, real: ClassId, proxy: ClassId) -> bool {
        if real == proxy {
            return true;
        }
        if !self.symbols().instance_of(proxy, real) {
            return false;
        }
        let names = |class: ClassId| {
            let mut names: Vec<Box<[u8]>> = Vec::new();
            for decl in self
                .symbols()
                .lineage(class)
                .filter_map(|here| self.symbols().class_decl(here))
            {
                for p in &decl.properties {
                    if !names.contains(&p.name.0) {
                        names.push(p.name.0.clone());
                    }
                }
            }
            names.len()
        };
        names(real) == names(proxy)
            && self.destructor(real) == self.destructor(proxy)
            && self.symbols().method_named(real, b"__clone")
                == self.symbols().method_named(proxy, b"__clone")
    }

    /// The `exit:` both arms of `zend_lazy_object_init` share: the owner
    /// the initialisation held given up, and php's `Error` where it was
    /// the last one, the object then freed.
    fn lazy_settle(
        &mut self,
        object: PhpObject,
        outcome: Result<PhpObject, Thrown>,
    ) -> Result<PhpObject, Thrown> {
        if object.heap().refcount() > 1 {
            self.release(object.as_value());
            return outcome;
        }
        let thrown = throw_error(
            self,
            "Error",
            "Lazy object was released during initialization",
        );
        // `zend_throw_error` over a pending throwable chains it as the
        // new one's previous.
        if let (Some(new), Err(earlier)) = (thrown.exception().copied(), &outcome)
            && let Some(earlier) = earlier.exception().copied()
        {
            self.set_previous(new, earlier);
        }
        self.release(object.as_value());
        Err(thrown)
    }

    /// `zend_lazy_object_mark_as_initialized`: each waiting slot given
    /// its default, the initializer never served, and the object no
    /// longer lazy.
    pub(crate) fn lazy_mark_initialized(&mut self, object: PhpObject) -> Result<(), Thrown> {
        let o = object.heap();
        let Some((_, lazy)) = self.lazy_waiting(o) else {
            return Ok(());
        };
        let defaults = self.property_defaults(object.class())?;
        o.set_laziness(Laziness::Eager);
        for (i, default) in (0..o.slots()).zip(defaults) {
            let Some(value) = default else { continue };
            if lazy.get(i as usize).copied().unwrap_or(false) {
                Heap::object_set_slot(&mut Hooks(self), o, i, value.into_zval());
            } else {
                self.release(value);
            }
        }
        self.lazy_forget(o);
        Ok(())
    }

    /// `ReflectionProperty::skipLazyInitialization` and
    /// `setRawValueWithoutLazyInitialization` over the object the
    /// initialised proxies lead to: the declared property `name` takes
    /// its default, or `value` written without the class's `__set`, and
    /// no longer waits; an object with no slot left waiting is realised,
    /// `zend_lazy_object_realize`. A throwing write leaves the slot
    /// waiting, as php flags it lazy again. The write is
    /// `write_property`'s, so a readonly property the first write
    /// initialised refuses the second.
    ///
    /// ```php
    /// class C { public readonly int $a; }
    /// $r = new ReflectionClass(C::class);
    /// $g = $r->newLazyGhost(function () { echo "init\n"; });
    /// $r->getProperty('a')->setRawValueWithoutLazyInitialization($g, 1);
    /// try { $r->getProperty('a')->setRawValueWithoutLazyInitialization($g, 2); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($g->a);
    /// ```
    /// ```expect
    /// Cannot modify readonly property C::$a
    /// int(1)
    /// ```
    pub(crate) fn lazy_skip(
        &mut self,
        object: PhpObject,
        name: &[u8],
        value: Option<Owner<Value>>,
    ) -> Result<(), Thrown> {
        let object = self.lazy_reader(object);
        let o = object.heap();
        let Some(slot) = self.prop_slot(object.class(), name) else {
            if let Some(value) = value {
                value.give_up(self);
            }
            return Ok(());
        };
        let was_lazy = self.lazy_slot(o, slot);
        let written = match value {
            None => {
                if !was_lazy {
                    return Ok(());
                }
                self.lazy_set_waiting(o, slot, false);
                let defaults = self.property_defaults(object.class())?;
                for (i, default) in defaults.into_iter().enumerate() {
                    let Some(default) = default else { continue };
                    if i == slot as usize {
                        Heap::object_set_slot(&mut Hooks(self), o, slot, default.into_zval());
                    } else {
                        self.release(default);
                    }
                }
                Ok(())
            }
            Some(value) => {
                self.lazy_set_waiting(o, slot, false);
                let key = match self.string(name) {
                    Ok(key) => key,
                    Err(thrown) => {
                        value.give_up(self);
                        return Err(thrown);
                    }
                };
                let written = self.in_declared_scope(object.class(), name, |req| {
                    // `write_property`'s `found` arm over a slot that
                    // holds a value: an initialised readonly property
                    // is not written again.
                    if !o.slot(slot).is_undef()
                        && let Some(placed) = req.declared_slot(object.class(), name)
                        && let Some(thrown) = req.found_refusal(o, &placed, name)
                    {
                        value.give_up(req);
                        req.release(Value::string(key));
                        return Err(thrown);
                    }
                    // The handler layer's key, named for the write.
                    req.write_std_property(object, Owner::adopt(key), value)
                });
                if written.is_err()
                    && was_lazy
                    && o.slot(slot).is_undef()
                    && matches!(o.laziness(), Laziness::Ghost | Laziness::Proxy)
                {
                    self.lazy_set_waiting(o, slot, true);
                }
                written
            }
        };
        if was_lazy
            && !self.lazy_slot(o, slot)
            && matches!(o.laziness(), Laziness::Ghost | Laziness::Proxy)
            && self
                .lazy_waiting(o)
                .is_some_and(|(_, lazy)| !lazy.contains(&true))
        {
            o.set_laziness(Laziness::Eager);
            self.lazy_forget(o);
        }
        written
    }

    fn lazy_set_waiting(&mut self, o: Obj, slot: u32, waiting: bool) {
        if let Some(Info {
            state: State::Waiting { lazy, .. },
            ..
        }) = self.lazy.rows.get_mut(&o.id().0)
            && let Some(at) = lazy.get_mut(slot as usize)
        {
            *at = waiting;
        }
    }

    /// `ReflectionProperty::isLazy`: whether the declared property
    /// `name` of the object the initialised proxies lead to waits.
    pub(crate) fn lazy_property(&self, object: PhpObject, name: &[u8]) -> bool {
        let object = self.lazy_reader(object);
        self.prop_slot(object.class(), name)
            .is_some_and(|slot| self.lazy_slot(object.heap(), slot))
    }

    /// `zend_lazy_object_clone` (`Z/Zend/zend_lazy_objects.c:711`): the
    /// object initialised first; a ghost is then cloned as any object
    /// is, and a proxy is cloned as a new proxy of the same class
    /// forwarding to a clone of its instance, `__clone` served on the
    /// instance's clone and never on the proxy.
    ///
    /// ```php
    /// class C { public $a = 1; public function __clone() { echo "clone\n"; } }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function ($o) { echo "init\n"; $o->a = 2; });
    /// var_dump((array) $o, get_mangled_object_vars($o));
    /// foreach ($o as $k => $v) { var_dump($k, $v); }
    /// $p = $r->newLazyProxy(function () { echo "factory\n"; return new C; });
    /// $c = clone $p;
    /// var_dump($c, $c == $p, $r->newLazyGhost(function ($o) { echo "init g\n"; }) == new C);
    /// ```
    /// ```expect
    /// array(0) {
    /// }
    /// array(0) {
    /// }
    /// init
    /// string(1) "a"
    /// int(2)
    /// factory
    /// clone
    /// init g
    /// lazy proxy object(C)#2 (1) {
    ///   ["instance"]=>
    ///   object(C)#6 (1) {
    ///     ["a"]=>
    ///     int(1)
    ///   }
    /// }
    /// bool(true)
    /// bool(true)
    /// ```
    pub(crate) fn lazy_clone(&mut self, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
        if matches!(object.heap().laziness(), Laziness::Ghost | Laziness::Proxy) {
            self.lazy_init(object)?;
        }
        let Some(instance) = self.lazy_instance(object.heap()) else {
            return self.clone_members(object);
        };
        // The new proxy is made before the instance is cloned, as
        // `zend_objects_new` stands before `zend_objects_clone_obj`
        // there, so the two take their ids in php's order.
        let proxy = crate::api::call::new_object_made(self, object.class())?;
        let p = proxy.heap();
        for i in 0..p.slots() {
            Heap::object_set_slot(&mut Hooks(self), p, i, php_heap::Zval::UNDEF);
            p.set_slot_bit(i, php_heap::PropBit::Uninit, true);
        }
        let table = self.object_handlers(instance.heap());
        let copy = match table.clone_obj() {
            Some(clone_obj) => {
                self.serve_as_table(instance.class(), |req| clone_obj(req, instance))
            }
            None => Err(self.uncloneable(instance)),
        };
        let copy = match copy {
            Ok(copy) => copy,
            Err(thrown) => {
                self.release(proxy.as_value());
                return Err(thrown);
            }
        };
        let skip_on_serialize = self.lazy_skips_serialize(object.heap());
        p.set_laziness(Laziness::Forwarding);
        self.lazy.rows.insert(
            p.id().0,
            Info {
                state: State::Instance(copy.hand_on()),
                skip_on_serialize,
            },
        );
        Ok(Owner::adopt(proxy))
    }

    /// `zend_lazy_object_debug_info`: what `var_dump` shows of a lazy
    /// object without initialising it, the one row `instance` for a
    /// proxy that forwards; `None`, the members as they stand, for one
    /// that waits.
    ///
    /// ```php
    /// class C { public $a = 1; }
    /// $o = (new ReflectionClass('C'))->newLazyProxy(function () { echo "factory\n"; return new C; });
    /// debug_zval_dump($o);
    /// echo json_encode($o), "\n";
    /// ```
    /// ```expect
    /// lazy proxy object(C)#3 (0) refcount(2){
    /// }
    /// factory
    /// {"a":1}
    /// ```
    pub(crate) fn lazy_debug_info(
        &mut self,
        object: PhpObject,
    ) -> Result<Option<PhpArray>, Thrown> {
        let Some(instance) = self.lazy_instance(object.heap()) else {
            return Ok(None);
        };
        let mut rows = self.array()?;
        let key = crate::api::values::PhpString::new(self, b"instance")?.into_key();
        let instance = crate::api::values::Owner::take(self, instance.as_value());
        rows.set(self, key, instance)?;
        Ok(Some(rows))
    }
}
