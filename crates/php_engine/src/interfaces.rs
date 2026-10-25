//! `Zend/zend_interfaces.c` at 8.5.9: `Traversable`, `IteratorAggregate`,
//! `Iterator`, `Serializable` and `Countable` as
//! `zend_interfaces.stub.php` declares them, registered by
//! `zend_register_interfaces` before any other class, and the
//! `zend_object_iterator` a `foreach` or a `yield from` over a
//! `Traversable` object walks ([`ObjectIterator`]): `getIterator()` asked
//! until it answers an `Iterator` or a `Generator`
//! (`zend_user_it_get_new_iterator`), then the iterator functions of
//! what it answered, `zend_user_iterator`'s over an `Iterator`'s methods
//! or `zend_generator_iterator_functions` over a generator, `rewind` and
//! `valid` at the reset and `move_forward`, `valid`, `get_current_data`
//! and `get_current_key` at each fetch, in the order
//! `zend_fe_fetch_object_helper` calls them. `ArrayAccess` and
//! `Stringable` are `classes`'s.
//!
//! A class of the engine's or an extension's whose `get_iterator` php
//! writes in C (`ArrayIterator`, `SplFixedArray`) is walked the same way
//! here, through its methods: a native method does what php's iterator
//! function does, and a user override is served where php's
//! `zend_user_it_*` fallback serves it, so the order of the calls a
//! program sees is the same.

use php_macros::php_class;
use php_unit::decl::{Bare, ClassKind};

use crate::api::abi::{
    ClassIterator, CurrentPlace, DtorObj, ExtensionBuilder, GcTable, Instance, IteratorElement,
    ObjectHandlers, Refusals, This, Untyped,
};
use crate::api::raising::{Thrown, error, throw_exception_ex};
use crate::api::request::Request;
use crate::api::values::{ClassId, Owner, PhpObject, PhpString, Value};

/// `interface Traversable`, which a user class cannot implement alone.
#[php_class(interface)]
pub(crate) struct Traversable;

#[php_class]
impl Traversable {}

/// `interface IteratorAggregate extends Traversable`, its one method's
/// return tentative (`@tentative-return-type`), as every method of the
/// stub's interfaces here is where it declares a type.
#[php_class(interface, extends(Traversable))]
pub(crate) struct IteratorAggregate;

#[php_class]
impl IteratorAggregate {
    #[php(tentative)]
    fn getIterator(this: This<Self>) -> Instance<Traversable>;
}

/// `interface Iterator extends Traversable`, its five methods in the
/// stub's order. A `foreach` over an object of a class implementing it
/// walks it through those methods, and over an `IteratorAggregate` asks
/// for the iterator once per loop and asks again of what it answers
/// until that is an `Iterator`; `Countable` is what `count()` asks of an
/// object whose class has no `count_elements`, and what `WeakMap`
/// implements.
///
/// ```php
/// class It implements Iterator {
///     private $i = 0;
///     function current(): mixed { echo "current "; return $this->i * 10; }
///     function key(): mixed { echo "key "; return "k" . $this->i; }
///     function next(): void { echo "next "; $this->i = $this->i + 1; }
///     function rewind(): void { echo "rewind "; $this->i = 0; }
///     function valid(): bool { echo "valid "; return $this->i < 2; }
/// }
/// foreach (new It as $k => $v) echo "$k=$v\n";
/// foreach (new It as $v) echo "$v\n";
/// var_dump(new It instanceof Traversable, new It instanceof Iterator);
/// ```
/// ```expect
/// rewind valid current key k0=0
/// next valid current key k1=10
/// next valid rewind valid current 0
/// next valid current 10
/// next valid bool(true)
/// bool(true)
/// ```
///
/// An `IteratorAggregate` is asked for its iterator once per loop, and
/// asked again of what it answers until that is an `Iterator`.
///
/// ```php
/// class Box implements IteratorAggregate {
///     function __construct(private $inner) {}
///     function getIterator(): Traversable { echo "get "; return $this->inner; }
/// }
/// class Two implements Iterator {
///     private $i = 0;
///     function current(): mixed { return $this->i; }
///     function key(): mixed { return $this->i; }
///     function next(): void { $this->i = $this->i + 1; }
///     function rewind(): void { $this->i = 0; }
///     function valid(): bool { return $this->i < 2; }
/// }
/// foreach (new Box(new Box(new Two)) as $k => $v) echo "$k:$v;";
/// ```
/// ```expect
/// get get 0:0;1:1;
/// ```
///
/// `Countable` is what `count()` asks of an object whose class has no
/// `count_elements`, and what `WeakMap` implements.
///
/// ```php
/// class N implements Countable { function count(): int { return 3; } }
/// var_dump(count(new N), new N instanceof Countable);
/// ```
/// ```expect
/// int(3)
/// bool(true)
/// ```
#[php_class(interface, extends(Traversable))]
pub(crate) struct Iterator;

#[php_class]
impl Iterator {
    #[php(tentative)]
    fn current(this: This<Self>) -> Value;
    #[php(tentative)]
    fn next(this: This<Self>);
    #[php(tentative)]
    fn key(this: This<Self>) -> Value;
    #[php(tentative)]
    fn valid(this: This<Self>) -> bool;
    #[php(tentative)]
    fn rewind(this: This<Self>);
}

/// `interface Serializable`, whose two methods the stub leaves untyped.
#[php_class(interface)]
pub(crate) struct Serializable;

#[php_class]
impl Serializable {
    fn serialize(this: This<Self>) -> Untyped<Value>;
    fn unserialize(this: This<Self>, data: PhpString) -> Untyped<Value>;
}

/// `interface Countable`.
#[php_class(interface)]
pub(crate) struct Countable;

#[php_class]
impl Countable {
    #[php(tentative)]
    fn count(this: This<Self>) -> i64;
}

/// Whether `class` is `Traversable`, php's `ce->get_iterator` being set:
/// what sends a `foreach` over the object to the iterator and not to
/// its properties.
pub(crate) fn is_traversable(req: &Request, class: ClassId) -> bool {
    req.class_named(Bare::resolved(b"traversable"))
        .is_some_and(|traversable| req.symbols().instance_of(class, traversable))
        || req.handlers_of(class).own_iterator()
}

/// The name of php's `zend_iterator_class_entry`, `__iterator_wrapper`
/// (`Z/Zend/zend_iterators.c:59`), keyed with a leading `\0`: php never
/// adds the class to `EG(class_table)`, and the byte is the mark
/// `get_declared_classes()` skips a key by, so no program reaches it by
/// name or sees it in a listing.
pub(crate) const ITERATOR_WRAPPER: &[u8] = b"\0__iterator_wrapper";

/// Which `zend_object_iterator_funcs` serve an [`ObjectIterator`], php's
/// `iter->funcs`: the table the class's `get_iterator` chose, read at
/// every step and never again chosen.
#[derive(Debug, Clone, Copy, Default)]
enum Funcs {
    /// `zend_interface_iterator_funcs_iterator`, the `zend_user_it_*`
    /// functions: each step one method of the `Iterator` served.
    #[default]
    User,
    /// `zend_generator_iterator_functions`: each step the generator
    /// resumed or read as `Generator`'s own methods do it, its delegation
    /// tree followed to the generator whose body runs
    /// ([`crate::generator`]).
    Generator,
    /// An iterator a native class walks in C and hands out by
    /// reference, `spl_array_it_funcs`: each step the class's native
    /// method, which is what php's function does over the same state,
    /// and the element under a by-reference fetch the place the class's
    /// [`CurrentPlace`] answers.
    Places(CurrentPlace),
    /// The functions of an internal class's own `get_iterator`, the one
    /// its `getIterator()` exposes through `zend_create_internal_iterator_zval`
    /// (`date_period_it_*`, `spl_fixedarray_it_*`): each step the element
    /// the class's [`ObjectHandlers::iterator_element`] answers at the
    /// iterator's position, served in no frame of its own.
    Element,
    /// The functions of an internal class's own `get_iterator` over an
    /// `Iterator`, whose iterator keeps a state of its own
    /// ([`ClassIterator`], in the wrapper's [`ObjectIterator::class`]):
    /// `spl_dllist_it_funcs`, `spl_recursive_it_iterator_funcs`. Each
    /// step is that iterator's, served in no frame of its own.
    Class,
}

impl Funcs {
    /// `funcs->rewind`: `zend_user_it_rewind`, or
    /// `zend_generator_iterator_rewind`, which runs the generator to its
    /// first `yield` and refuses a generator already past it.
    fn rewind(self, req: &mut Request, it: Walk) -> Result<(), Thrown> {
        let Walk { wrapper, data } = it;
        match self {
            Funcs::User | Funcs::Places(_) => {
                invalidate_current(req, wrapper);
                let answered = crate::api::call::call_method(req, &data, b"rewind", &[])?;
                answered.give_up(req);
                Ok(())
            }
            Funcs::Generator => crate::generator::iterator_rewind(req, data),
            Funcs::Element => {
                set_position(req, wrapper, 0);
                Ok(())
            }
            Funcs::Class => {
                class_step(req, it, |iter, req, data| iter.rewind(req, data)).unwrap_or(Ok(()))
            }
        }
    }

    /// `funcs->valid`, `SUCCESS` as `true`: `zend_user_it_valid`'s
    /// `valid()` read as `i_zend_is_true` reads it, or whether the
    /// generator still has its frame.
    fn valid(self, req: &mut Request, it: Walk) -> Result<bool, Thrown> {
        let Walk { wrapper, data } = it;
        match self {
            Funcs::User | Funcs::Places(_) => {
                let answered = crate::api::call::call_method(req, &data, b"valid", &[])?;
                let valid = crate::api::coerce::truthy(req, &answered);
                answered.give_up(req);
                valid
            }
            Funcs::Generator => crate::generator::iterator_valid(req, data),
            Funcs::Element => Ok(element_stands(element(req, wrapper, data)?, req)),
            Funcs::Class => {
                class_step(req, it, |iter, req, data| iter.valid(req, data)).unwrap_or(Ok(false))
            }
        }
    }

    /// `funcs->move_forward`: `next()` served, or the generator resumed.
    fn move_forward(self, req: &mut Request, it: Walk) -> Result<(), Thrown> {
        let Walk { wrapper, data } = it;
        match self {
            Funcs::User | Funcs::Places(_) => {
                invalidate_current(req, wrapper);
                let answered = crate::api::call::call_method(req, &data, b"next", &[])?;
                answered.give_up(req);
                Ok(())
            }
            Funcs::Generator => crate::generator::iterator_move_forward(req, data),
            Funcs::Element => {
                let at = position(req, wrapper);
                set_position(req, wrapper, at.saturating_add(1));
                Ok(())
            }
            Funcs::Class => class_step(req, it, |iter, req, data| iter.move_forward(req, data))
                .unwrap_or(Ok(())),
        }
    }

    /// `funcs->get_current_data`, one owner the caller's: what
    /// `current()` answered, or the value the generator yielded with a
    /// reference followed. `by_ref` is `FE_FETCH_RW`'s
    /// `ZVAL_MAKE_REF` over the zval php's function points at: the
    /// generator's own value made a reference in place, the cell handed
    /// out; `current()`'s answer is no zval anyone else holds, and is
    /// handed out as it is for the fetch to wrap.
    fn current(self, req: &mut Request, it: Walk, by_ref: bool) -> Result<Value, Thrown> {
        let Walk { wrapper, data } = it;
        match self {
            Funcs::Places(place) if by_ref => {
                let placed = req.serve_as_table(data.class(), |req| place(req, data))?;
                Ok(placed.map_or_else(Value::null, |cell| cell.into_value().hand_on()))
            }
            Funcs::Places(_) => crate::api::call::call_method_made(req, &data, b"current", &[]),
            Funcs::User => {
                if let Some(kept) = wrapper.native::<Wrapped>(req).and_then(|it| it.value) {
                    req.addref(&kept);
                    return Ok(kept);
                }
                let value = crate::api::call::call_method_made(req, &data, b"current", &[])?;
                req.addref(&value);
                if let Some(it) = wrapper.native_mut::<Wrapped>(req) {
                    it.value = Some(value);
                }
                Ok(value)
            }
            Funcs::Generator if by_ref => {
                crate::generator::iterator_current_ref(req, data).map(|cell| cell.as_value())
            }
            Funcs::Generator => {
                let value = crate::generator::iterator_current(req, data)?
                    .deref()
                    .undef_as_null();
                req.addref(&value);
                Ok(value)
            }
            Funcs::Element => Ok(element_value(element(req, wrapper, data)?, req).hand_on()),
            Funcs::Class => class_step(req, it, |iter, req, data| {
                iter.current(req, data).map(Owner::hand_on)
            })
            .unwrap_or_else(|| Ok(Value::null())),
        }
    }

    /// `funcs->get_current_key`, one owner the caller's:
    /// `zend_user_it_get_current_key`'s `key()` with a reference
    /// unwrapped, or the generator's key, `null` where it has none.
    fn key(self, req: &mut Request, it: Walk) -> Result<Value, Thrown> {
        let Walk { wrapper, data } = it;
        match self {
            Funcs::User | Funcs::Places(_) => {
                let answered = crate::api::call::call_method(req, &data, b"key", &[])?;
                let bare = answered.deref();
                req.addref(&bare);
                answered.give_up(req);
                Ok(bare)
            }
            Funcs::Generator => crate::generator::iterator_key(req, data),
            Funcs::Element => Ok(element_key(element(req, wrapper, data)?, req).hand_on()),
            Funcs::Class => class_step(req, it, |iter, req, data| {
                iter.key(req, data).map(Owner::hand_on)
            })
            .unwrap_or_else(|| Ok(Value::null())),
        }
    }
}

/// `zend_user_it_invalidate_current`: the value `current()` answered
/// last given up, before a step that moves the walk.
pub(crate) fn invalidate_current(req: &mut Request, wrapper: PhpObject) {
    if let Some(kept) = wrapper
        .native_mut::<Wrapped>(req)
        .and_then(|it| it.value.take())
    {
        req.release(kept);
    }
}

/// One step of a [`Funcs::Class`] walk: the class's iterator taken out
/// of the wrapper for the length of the step, which may call PHP code
/// and so may not borrow the wrapper, and put back after it. `None`
/// where the wrapper holds none, which only a wrapper nothing made an
/// iterator of does; the step's own answer is the `Some`.
fn class_step<R>(
    req: &mut Request,
    it: Walk,
    step: impl FnOnce(&mut dyn ClassIterator, &mut Request, PhpObject) -> R,
) -> Option<R> {
    let Walk { wrapper, data } = it;
    let mut iter = wrapper
        .native_mut::<Wrapped>(req)
        .and_then(|it| it.class.take())?;
    let answered = step(iter.as_mut(), req, data);
    if let Some(it) = wrapper.native_mut::<Wrapped>(req) {
        it.class = Some(iter);
    }
    Some(answered)
}

/// The iterator an [`ObjectIterator`] is and the object it walks, what
/// each of the [`Funcs`] is handed: php's `zend_object_iterator *iter`,
/// whose `data` the functions read and whose own fields `Element` keeps
/// its position in.
#[derive(Clone, Copy)]
struct Walk {
    wrapper: PhpObject,
    data: PhpObject,
}

/// How many steps past its rewind an `Element` walk stands.
fn position(req: &Request, wrapper: PhpObject) -> usize {
    wrapper.native::<Wrapped>(req).map_or(0, |it| it.position)
}

/// The `Element` walk's position set.
fn set_position(req: &mut Request, wrapper: PhpObject, at: usize) {
    if let Some(it) = wrapper.native_mut::<Wrapped>(req) {
        it.position = at;
    }
}

/// The key and the value an `Element` walk stands at, through the walked
/// object's class's `iterator_element`, one owner each; none past the
/// last.
fn element(
    req: &mut Request,
    wrapper: PhpObject,
    data: PhpObject,
) -> Result<Option<IteratorElement>, Thrown> {
    let at = position(req, wrapper);
    let table = req.object_handlers(data.heap());
    table.iterator_element(req, data, at)
}

/// `zend_object_iterator` (`Z/Zend/zend_iterators.h`), the iterator a
/// `Traversable` object's `get_iterator` hands out: what a `foreach`
/// over the object holds in its loop variable from the reset to
/// `FE_FREE`, as `zend_fe_reset_iterator` puts `&iter->std` there, and
/// what a `yield from` over it holds in the generator's delegated values.
/// It is the payload of an object of the `__iterator_wrapper` class
/// ([`ITERATOR_WRAPPER`]), so its holder owns it as it owns any value:
/// the release at `FE_FREE`, at the unwind of the loop's live range or
/// when the generator gives its values up, gives back the object it
/// walks, and it takes the number in the object store
/// `zend_iterator_init` takes, which a `var_dump` inside the body shows.
/// The progress is `index`, never the loop variable's `u2` word.
///
/// Two tables serve it ([`Funcs`]): `zend_user_iterator`'s over an
/// `Iterator`'s methods, and `zend_generator_iterator_functions` over a
/// `Generator`, and a third over an internal class whose `getIterator()`
/// hands out the `InternalIterator` of its own C iterator ([`Funcs::Element`]).
/// Any other class php walks with an iterator of its own in C
/// (`zend_class_entry.get_iterator`, #735) is walked through its methods
/// here, so it is the first table's.
#[derive(Default)]
pub(crate) struct ObjectIterator {
    /// `iter->funcs`.
    funcs: Funcs,
    /// `it.data`: the `Iterator` or the `Generator` walked, one owner;
    /// `None` only in an object nothing made an iterator of.
    data: Option<PhpObject>,
    /// `iter->index`: the fetches made, the first counting none; `None`
    /// before the first fetch, php's `-1` after the reset and `0` after a
    /// `yield from`, which moves nothing forward.
    index: Option<u64>,
    /// The steps an `Element` walk has moved since its rewind, what
    /// `date_period_it`'s `current_index` counts.
    position: usize,
    /// A `Class` walk's iterator, the state php's `spl_dllist_it`
    /// extends `zend_object_iterator` with.
    class: Option<Box<dyn ClassIterator>>,
    /// `zend_user_iterator.value`: what a `User` walk's `current()`
    /// answered, one owner, kept until the walk moves, so asking twice
    /// between two steps serves `current()` once.
    value: Option<Value>,
}

/// The wrapper's payload: the [`ObjectIterator`] boxed, so the object is
/// the size of the struct php's `get_iterator` `emalloc`s for its kind
/// ([`iterator_size`]) and the state fits inside it (design §5.13).
pub(crate) type Wrapped = Box<ObjectIterator>;

/// `sizeof` what php's `get_iterator` `emalloc`s for an iterator served
/// by `funcs` over `data`: `zend_user_iterator`, 112 bytes, for an
/// `Iterator`'s methods (`zend_user_it_get_iterator`); the bare
/// `zend_object_iterator`, 88, for a `Generator`
/// (`zend_generator_get_iterator`); and for a class that walks its
/// objects in C, the struct its handlers name
/// ([`ObjectHandlers::iterator_size`]).
///
/// ```php
/// function g() { yield 1; }
/// class I implements Iterator { function current(): mixed { return 1; } function key(): mixed { return 0; }
///     function next(): void {} function rewind(): void {} function valid(): bool { return false; } }
/// $a = new ArrayIterator([]); $i = new I; $gen = g(); $w = [new IteratorIterator($a), new IteratorIterator($i), new IteratorIterator($gen)];
/// $m = memory_get_usage(); $x = new IteratorIterator($a); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $y = new IteratorIterator($i); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $z = new IteratorIterator($gen); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 288
/// 304
/// 288
/// ```
fn iterator_size(req: &Request, funcs: Funcs, data: PhpObject) -> usize {
    const ZEND_USER_ITERATOR: usize = 112;
    const ZEND_OBJECT_ITERATOR: usize = 88;
    match funcs {
        Funcs::User => ZEND_USER_ITERATOR,
        Funcs::Generator => ZEND_OBJECT_ITERATOR,
        Funcs::Places(_) | Funcs::Element | Funcs::Class => {
            req.object_handlers(data.heap()).iterator_size()
        }
    }
}

/// `iterator_object_handlers` (`Z/Zend/zend_iterators.c:28`):
/// `iter_wrapper_free` is `funcs->dtor`, which for both tables gives back
/// the object walked, `iter_wrapper_dtor` serves nothing,
/// `iter_wrapper_get_gc` names what the iterator holds, and the object is
/// uncloneable.
struct WrapperHandlers;

static WRAPPER: WrapperHandlers = WrapperHandlers;

impl ObjectHandlers for WrapperHandlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        invalidate_current(req, object);
        let (held, class_iter) = object
            .native_mut::<Wrapped>(req)
            .map(|it| (it.data.take(), it.class.take()))
            .unwrap_or_default();
        if let (Some(held), Some(mut class_iter)) = (held, class_iter) {
            class_iter.release(req, held);
        }
        if let Some(held) = held {
            req.release(held.as_value());
        }
    }

    fn dtor_obj(&self) -> DtorObj {
        DtorObj::Own(|_, _| {})
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(it) = object.native::<Wrapped>(req) {
            if let Some(held) = it.data {
                visit(held.as_value());
            }
            if let Some(kept) = it.value {
                visit(kept);
            }
        }
        GcTable::Properties
    }

    fn refusals(&self) -> Refusals {
        Refusals::NONE.cloning()
    }
}

/// `zend_register_iterator_wrapper`'s class onto `builder`: final, of no
/// member, its objects carrying an [`ObjectIterator`]. php keeps it out
/// of `EG(class_table)` and no stub declares it, so it is the one class of
/// Core's a header does not name.
pub(crate) fn iterator_wrapper(builder: ExtensionBuilder) -> ExtensionBuilder {
    let mut decl = crate::api::__private::class(ITERATOR_WRAPPER, ClassKind::Class);
    decl.flags.is_final = true;
    builder.class_created(
        decl,
        Vec::new(),
        &WRAPPER,
        crate::api::abi::new_payload::<Wrapped>(),
    )
}

/// `zend_iterator_unwrap` (`Z/Zend/zend_iterators.c:98`) as the test it
/// is in `FE_FETCH_*`: whether `object` is an iterator's wrapper, php's
/// comparison of the object's handler table, here of its payload's type.
pub(crate) fn is_iterator_wrapper(req: &Request, object: PhpObject) -> bool {
    object.native::<Wrapped>(req).is_some()
}

/// `ce->get_iterator(ce, object, by_ref)` over a `Traversable` object
/// ([`is_traversable`]): the wrapper of a fresh [`ObjectIterator`], one
/// owner the caller's. A `Generator`'s is `zend_generator_get_iterator`,
/// which refuses a closed generator and one walked by reference that
/// does not yield by reference ([`crate::generator::get_iterator`]); an
/// `Iterator`'s is `zend_user_it_get_iterator`, which refuses any walk by
/// reference; an `IteratorAggregate`'s is `zend_user_it_get_new_iterator`
/// (`Z/Zend/zend_interfaces.c:179`), `getIterator()` asked and what it
/// answered asked in turn with the same `by_ref`, php's `Exception`
/// naming the class where the answer is not `Traversable` or is the
/// object itself; and an internal class's own, where its native
/// `getIterator()` answers the `InternalIterator` over the object
/// ([`Funcs::Element`]), which refuses any walk by reference. On a throw
/// nothing is held.
///
/// ```php
/// class Gen implements IteratorAggregate {
///     function getIterator(): Generator { yield 'a' => 1; yield 'b' => 2; }
/// }
/// foreach (new Gen as $k => $v) echo "$k=$v ";
/// class ByRef implements IteratorAggregate {
///     public $a = [1, 2];
///     function &getIterator(): Generator { foreach ($this->a as &$v) yield $v; }
/// }
/// $o = new ByRef;
/// foreach ($o as &$v) $v *= 10;
/// unset($v);
/// echo implode(',', $o->a), "\n";
/// class Me implements IteratorAggregate { function getIterator(): Traversable { return $this; } }
/// try { foreach (new Me as $v) {} } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// class NoCtor extends DatePeriod { function __construct() {} }
/// try { foreach (new NoCtor as $d) {} } catch (Error $e) { echo count($e->getTrace()), ' ', $e->getMessage(), "\n"; }
/// try { foreach (SplFixedArray::fromArray([1]) as &$v) {} } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// foreach (SplFixedArray::fromArray(['x', 'y']) as $k => $v) echo "$k=$v ";
/// ```
/// ```expect
/// a=1 b=2 10,20
/// Objects returned by Me::getIterator() must be traversable or implement interface Iterator
/// 0 Object of type DatePeriod has not been correctly initialized by calling parent::__construct() in its constructor
/// An iterator cannot be used with foreach by reference
/// 0=x 1=y
/// ```
pub(crate) fn get_iterator(
    req: &mut Request,
    object: PhpObject,
    by_ref: bool,
) -> Result<PhpObject, Thrown> {
    let (funcs, data, class_iter) = iterated(req, object, by_ref)?;
    wrap(req, funcs, data, class_iter)
}

/// `zend_iterator_init`: the iterator over `data`, one owner the
/// iterator's, taken from the caller, in the object store as php keeps it;
/// then a class's own iterator's [`ClassIterator::made`], the rest of its
/// `get_iterator`, whose throw releases the iterator as
/// `zend_iterator_dtor` does.
fn wrap(
    req: &mut Request,
    funcs: Funcs,
    data: PhpObject,
    class_iter: Option<Box<dyn ClassIterator>>,
) -> Result<PhpObject, Thrown> {
    let size = iterator_size(req, funcs, data);
    let wrapper = match req.class_named(Bare::resolved(ITERATOR_WRAPPER)) {
        Some(class) => req.new_object_sized(
            class,
            Box::new(ObjectIterator {
                funcs,
                data: Some(data),
                index: None,
                position: 0,
                class: class_iter,
                value: None,
            }),
            size,
        ),
        None => Err(crate::api::raising::fatal(
            req,
            "InternalError: the iterator wrapper class is not declared",
        )),
    };
    let wrapper = wrapper.inspect_err(|_| req.release(data.as_value()))?;
    let it = Walk { wrapper, data };
    if let Some(Err(thrown)) = class_step(req, it, |iter, req, data| iter.made(req, data)) {
        req.release(wrapper.as_value());
        return Err(thrown);
    }
    Ok(wrapper)
}

/// The table [`get_iterator`] chooses for `object`, the object it
/// walks, one owner the caller's, and the iterator of a class that
/// walks its objects with one of its own ([`Funcs::Class`]).
type Iterated = (Funcs, PhpObject, Option<Box<dyn ClassIterator>>);

/// The table [`get_iterator`] chooses for `object` and what it walks
/// ([`Iterated`]).
fn iterated(req: &mut Request, object: PhpObject, by_ref: bool) -> Result<Iterated, Thrown> {
    if crate::generator::is_generator(req, object) {
        crate::generator::get_iterator(req, object, by_ref)?;
        req.addref(&object.as_value());
        return Ok((Funcs::Generator, object, None));
    }
    // `ce->get_iterator` on a class that is no `Traversable`: its own
    // iterator, and nothing else, walks it.
    let table = req.object_handlers(object.heap());
    if table.own_iterator() {
        return match table.get_iterator(req, object, by_ref)? {
            Some(class_iter) => {
                req.addref(&object.as_value());
                Ok((Funcs::Class, object, Some(class_iter)))
            }
            None => Err(no_foreach_by_ref(req)),
        };
    }
    if req
        .class_named(Bare::resolved(b"iterator"))
        .is_some_and(|iterator| req.symbols().instance_of(object.class(), iterator))
    {
        if iterator_methods_native(req, object) {
            let table = req.object_handlers(object.heap());
            if let Some(class_iter) = table.get_iterator(req, object, by_ref)? {
                req.addref(&object.as_value());
                return Ok((Funcs::Class, object, Some(class_iter)));
            }
        }
        let funcs = match by_ref_places(req, object) {
            Some(place) => Funcs::Places(place),
            None if by_ref => {
                return Err(no_foreach_by_ref(req));
            }
            None => Funcs::User,
        };
        req.addref(&object.as_value());
        return Ok((funcs, object, None));
    }
    // `zend_implement_aggregate`: an internal class keeps its own
    // `get_iterator` while `getIterator()` is the one it declared, and
    // that method is `zend_create_internal_iterator_zval` over the
    // object, so its answer names the iterator the class walks with.
    let native = req
        .symbols()
        .method_named(object.class(), b"getiterator")
        .is_some_and(|function| req.user_function(function).is_none());
    if native
        && let Some(class_iter) = req
            .object_handlers(object.heap())
            .get_iterator(req, object, by_ref)?
    {
        req.addref(&object.as_value());
        return Ok((Funcs::Class, object, Some(class_iter)));
    }
    let answered = crate::api::call::call_method(req, &object, b"getIterator", &[])?;
    match answered.deref().as_object() {
        Some(inner) if native && internal_walked(req, inner) == Some(object) => {
            answered.give_up(req);
            if by_ref {
                return Err(no_foreach_by_ref(req));
            }
            req.addref(&object.as_value());
            Ok((Funcs::Element, object, None))
        }
        Some(inner) if inner != object && is_traversable(req, inner.class()) => {
            let walked = iterated(req, inner, by_ref);
            answered.give_up(req);
            walked
        }
        _ => {
            answered.give_up(req);
            let message = bytes!(
                b"Objects returned by ",
                req.symbols().class_name(object.class()),
                b"::getIterator() must be traversable or implement interface Iterator"
            );
            Err(throw_exception_ex(req, ClassId::EXCEPTION, &message))
        }
    }
}

/// The places an `Iterator` object's own iterator hands out, where its
/// class walks it in C (`ce->get_iterator` a native's): the table's
/// [`ObjectHandlers::current_place`], kept only while none of the five
/// `Iterator` methods is a user's, since `zend_implement_iterator` puts
/// `zend_user_it_get_iterator` in place of the native one on a class
/// that overrides any of them.
fn by_ref_places(req: &Request, object: PhpObject) -> Option<CurrentPlace> {
    let place = req.object_handlers(object.heap()).current_place()?;
    iterator_methods_native(req, object).then_some(place)
}

/// Whether none of the five `Iterator` methods of the object's class is
/// a user's, which is when `zend_implement_iterator` leaves the class
/// the `get_iterator` it inherits from an internal class.
fn iterator_methods_native(req: &Request, object: PhpObject) -> bool {
    [b"rewind".as_slice(), b"valid", b"key", b"current", b"next"]
        .iter()
        .all(|name| {
            req.symbols()
                .method_named(object.class(), name)
                .is_some_and(|function| req.user_function(function).is_none())
        })
}

/// `zend_fe_reset_iterator` (`Z/Zend/zend_execute.c:5368`) over a
/// `Traversable` object: the iterator [`get_iterator`] makes, then
/// `funcs->rewind` and `funcs->valid`. The wrapper comes back with one
/// owner, the loop's, and whether `valid` answered `false`, php's
/// `is_empty`, which jumps past the loop; on a throw nothing is held.
pub(crate) fn reset(
    req: &mut Request,
    object: PhpObject,
    by_ref: bool,
) -> Result<(PhpObject, bool), Thrown> {
    let wrapper = get_iterator(req, object, by_ref)?;
    let Some((funcs, data)) = walked(req, wrapper) else {
        return Ok((wrapper, true));
    };
    let it = Walk { wrapper, data };
    let valid = funcs.rewind(req, it).and_then(|()| funcs.valid(req, it));
    match valid {
        Ok(valid) => Ok((wrapper, !valid)),
        Err(thrown) => Err(req.free_pending(thrown, |req| req.release(wrapper.as_value()))),
    }
}

/// The table and the object walked of the iterator `wrapper` holds, the
/// index moved on as `++iter->index` moves it, and whether this is a
/// fetch past the first, which moves the walk forward first.
fn step(req: &mut Request, wrapper: PhpObject) -> Option<(Funcs, PhpObject, bool)> {
    let it = wrapper.native_mut::<Wrapped>(req)?;
    let data = it.data?;
    let fetched = it.index;
    it.index = Some(fetched.map_or(0, |index| index.saturating_add(1)));
    Some((it.funcs, data, fetched.is_some()))
}

/// The table and the object walked of the iterator `wrapper` holds.
fn walked(req: &Request, wrapper: PhpObject) -> Option<(Funcs, PhpObject)> {
    let it = wrapper.native::<Wrapped>(req)?;
    it.data.map(|data| (it.funcs, data))
}

/// The iterator arm of `zend_fe_fetch_object_helper`
/// (`Z/Zend/zend_vm_def.h:7122`) and of `ZEND_FE_FETCH_RW`: past the
/// first fetch `funcs->move_forward` and `funcs->valid`; then
/// `funcs->get_current_data`, by reference under `by_ref`
/// ([`Funcs::current`]), and `funcs->get_current_key` where the opline
/// keeps the key. `None` is the end, the fetch's jump past the loop. The
/// values are the caller's, one owner each.
pub(crate) fn fetch(
    req: &mut Request,
    wrapper: PhpObject,
    keyed: bool,
    by_ref: bool,
) -> Result<Option<(Value, Option<Value>)>, Thrown> {
    let Some((funcs, data, moves)) = step(req, wrapper) else {
        return Ok(None);
    };
    let it = Walk { wrapper, data };
    if moves {
        funcs.move_forward(req, it)?;
        if !funcs.valid(req, it)? {
            return Ok(None);
        }
    }
    let current = funcs.current(req, it, by_ref)?;
    if !keyed {
        return Ok(Some((current, None)));
    }
    match funcs.key(req, it) {
        Ok(key) => Ok(Some((current, Some(key)))),
        Err(thrown) => {
            req.release(current);
            Err(thrown)
        }
    }
}

/// `zend_generator_get_next_delegated_value`'s iterator arm
/// (`Z/Zend/zend_generators.c:669`), the walk a `yield from` over a
/// `Traversable` makes of the iterator [`get_iterator`] handed it: past
/// the first element `funcs->move_forward`, then `funcs->valid` every
/// time, since `YIELD_FROM` rewound without asking; the value and the
/// key, one owner each, or `None` where the walk is spent.
pub(crate) fn delegated(
    req: &mut Request,
    wrapper: PhpObject,
) -> Result<Option<(Value, Value)>, Thrown> {
    let Some((funcs, data, moves)) = step(req, wrapper) else {
        return Ok(None);
    };
    let it = Walk { wrapper, data };
    if moves {
        funcs.move_forward(req, it)?;
    }
    if !funcs.valid(req, it)? {
        return Ok(None);
    }
    let current = funcs.current(req, it, false)?;
    match funcs.key(req, it) {
        Ok(key) => Ok(Some((current, key))),
        Err(thrown) => {
            req.release(current);
            Err(thrown)
        }
    }
}

/// `iter->funcs->valid` over the iterator `wrapper` holds, `false` for
/// a wrapper nothing made an iterator of.
pub(crate) fn valid(req: &mut Request, wrapper: PhpObject) -> Result<bool, Thrown> {
    match walked(req, wrapper) {
        Some((funcs, data)) => funcs.valid(req, Walk { wrapper, data }),
        None => Ok(false),
    }
}

/// `iter->funcs->get_current_data`, one owner the caller's.
pub(crate) fn current(req: &mut Request, wrapper: PhpObject) -> Result<Value, Thrown> {
    match walked(req, wrapper) {
        Some((funcs, data)) => funcs.current(req, Walk { wrapper, data }, false),
        None => Ok(Value::null()),
    }
}

/// `iter->funcs->get_current_key`, one owner the caller's.
pub(crate) fn key(req: &mut Request, wrapper: PhpObject) -> Result<Value, Thrown> {
    match walked(req, wrapper) {
        Some((funcs, data)) => funcs.key(req, Walk { wrapper, data }),
        None => Ok(Value::null()),
    }
}

/// `iter->funcs->move_forward`.
pub(crate) fn move_forward(req: &mut Request, wrapper: PhpObject) -> Result<(), Thrown> {
    match walked(req, wrapper) {
        Some((funcs, data)) => funcs.move_forward(req, Walk { wrapper, data }),
        None => Ok(()),
    }
}

/// `iter->funcs->rewind` over the iterator `wrapper` holds, what
/// `YIELD_FROM` does to the iterator it takes before it stores it.
pub(crate) fn rewind(req: &mut Request, wrapper: PhpObject) -> Result<(), Thrown> {
    match walked(req, wrapper) {
        Some((funcs, data)) => funcs.rewind(req, Walk { wrapper, data }),
        None => Ok(()),
    }
}

/// `zend_internal_iterator`: what an `InternalIterator` walks. A class
/// with an iterator of its own (`ce->get_iterator`) walks with it, held in
/// the object store as php holds it, rewound once, php's `rewind_called`;
/// any other walks its elements by index.
#[derive(Default)]
pub(crate) enum Internal {
    #[default]
    Unmade,
    Elements {
        object: PhpObject,
        // Four bytes, so the payload fits `zend_internal_iterator`.
        index: u32,
    },
    Own {
        iter: PhpObject,
        rewound: bool,
    },
}

/// The class's own walk an `InternalIterator` holds, rewound first where
/// `ensure` asks, as `zend_internal_iterator_ensure_rewound` rewinds it;
/// `None` for a walk by index.
fn internal_walk(
    req: &mut Request,
    this: PhpObject,
    ensure: bool,
) -> Option<Result<(Funcs, Walk), Thrown>> {
    let Some(Internal::Own { iter, rewound }) = this.native_mut::<Internal>(req) else {
        return None;
    };
    let (wrapper, first) = (*iter, ensure && !*rewound);
    *rewound |= ensure;
    let (funcs, data) = walked(req, wrapper)?;
    let walk = Walk { wrapper, data };
    Some(if first {
        funcs.rewind(req, walk).map(|()| (funcs, walk))
    } else {
        Ok((funcs, walk))
    })
}

/// `zend_internal_iterator_handlers`: `free_obj` gives back the object
/// walked, `get_gc` names it, and the class is uncloneable.
struct InternalHandlers;

static INTERNAL: InternalHandlers = InternalHandlers;

impl ObjectHandlers for InternalHandlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let held = object.native_mut::<Internal>(req).map(std::mem::take);
        if let Some(Internal::Elements { object: held, .. } | Internal::Own { iter: held, .. }) =
            held
        {
            req.release(held.as_value());
        }
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(Internal::Elements { object: held, .. } | Internal::Own { iter: held, .. }) =
            object.native::<Internal>(req)
        {
            visit(held.as_value());
        }
        GcTable::Properties
    }

    fn refusals(&self) -> Refusals {
        Refusals::NONE.cloning()
    }
}

/// `final class InternalIterator implements Iterator` as
/// `zend_interfaces.stub.php` declares it, `@not-serializable`, its
/// constructor private, its objects carrying an [`Internal`].
///
/// ```php
/// $f = SplFixedArray::fromArray(['a', 'b']);
/// $it = $f->getIterator();
/// var_dump(get_class($it), $it instanceof Iterator);
/// foreach ($it as $k => $v) echo "$k=$v ";
/// $it->rewind(); $it->next(); var_dump($it->key(), $it->current(), $it->valid());
/// $it->next(); var_dump($it->valid());
/// ```
/// ```expect
/// string(16) "InternalIterator"
/// bool(true)
/// 0=a 1=b int(1)
/// string(1) "b"
/// bool(true)
/// bool(false)
/// ```
#[php_class(final, not_serializable, implements(Iterator), size = 72, payload = Internal, handlers = INTERNAL)]
pub(crate) struct InternalIterator;

#[php_class]
impl InternalIterator {
    /// `InternalIterator::__construct`: php's `Error`, whatever it was
    /// handed, since php's body parses no parameter.
    ///
    /// ```php
    /// try { (new ReflectionMethod('InternalIterator', '__construct'))->invoke(SplFixedArray::fromArray([1])->getIterator(), 'extra'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot manually construct InternalIterator
    /// ```
    #[php(private, arity = manual)]
    fn __construct(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let _ = this;
        Err(error(req, "Cannot manually construct InternalIterator"))
    }

    /// `InternalIterator::current(): mixed`.
    fn current(req: &mut Request, this: This<Self>) -> Result<Owner<Value>, Thrown> {
        if let Some(walk) = internal_walk(req, this.object(), true) {
            let (funcs, walk) = walk?;
            let made = funcs.current(req, walk, false);
            return crate::api::values::adopted(made);
        }
        Ok(element_value(internal_element(req, this.object())?, req))
    }

    /// `InternalIterator::key(): mixed`.
    fn key(req: &mut Request, this: This<Self>) -> Result<Owner<Value>, Thrown> {
        if let Some(walk) = internal_walk(req, this.object(), true) {
            let (funcs, walk) = walk?;
            let made = funcs.key(req, walk);
            return crate::api::values::adopted(made);
        }
        Ok(element_key(internal_element(req, this.object())?, req))
    }

    /// `InternalIterator::next(): void`.
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        if let Some(walk) = internal_walk(req, this.object(), true) {
            let (funcs, walk) = walk?;
            return funcs.move_forward(req, walk);
        }
        if let Some(Internal::Elements { index, .. }) = this.object().native_mut::<Internal>(req) {
            *index = index.saturating_add(1);
        }
        Ok(())
    }

    /// `InternalIterator::valid(): bool`.
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        if let Some(walk) = internal_walk(req, this.object(), true) {
            let (funcs, walk) = walk?;
            return funcs.valid(req, walk);
        }
        Ok(element_stands(internal_element(req, this.object())?, req))
    }

    /// `InternalIterator::rewind(): void`: the class's own iterator
    /// rewound, which one with no rewind of its own refuses past its
    /// first step, php's `Iterator does not support rewinding`.
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        if let Some(walk) = internal_walk(req, this.object(), false) {
            let (funcs, walk) = walk?;
            if let Some(Internal::Own { rewound, .. }) = this.object().native_mut::<Internal>(req) {
                *rewound = true;
            }
            return funcs.rewind(req, walk);
        }
        if let Some(Internal::Elements { index, .. }) = this.object().native_mut::<Internal>(req) {
            *index = 0;
        }
        Ok(())
    }
}

/// The object an `InternalIterator` walks, none for any other object.
fn internal_walked(req: &Request, iterator: PhpObject) -> Option<PhpObject> {
    match iterator.native::<Internal>(req)? {
        Internal::Elements { object, .. } => Some(*object),
        Internal::Own { iter, .. } => walked(req, *iter).map(|(_, data)| data),
        Internal::Unmade => None,
    }
}

/// `zend_create_internal_iterator_zval`: the class's own iterator over
/// `object`, where it has one, made first, as php makes it, then the
/// `InternalIterator` holding it; a class walked by index is held itself.
/// The `InternalIterator` is `zend_internal_iterator`, `emalloc`'d whole,
/// and the iterator is the struct the class's `get_iterator` makes
/// ([`ObjectHandlers::iterator_size`]):
///
/// ```php
/// $per = new DatePeriod(new DateTime('2020-01-01'), new DateInterval('P1D'), 2); $w3 = $per->getIterator();
/// $f = SplFixedArray::fromArray([1, 2]); $w4 = $f->getIterator();
/// $wm = new WeakMap; $w5 = $wm->getIterator();
/// $m = memory_get_usage(); $a = $f->getIterator(); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $b = $wm->getIterator(); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); unset($a, $b); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 176
/// 176
/// -352
/// ```
pub(crate) fn internal_iterator(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    const ZEND_INTERNAL_ITERATOR: usize = 72;
    let Some(class) = req.class_named(Bare::resolved(b"internaliterator")) else {
        return Err(crate::api::raising::fatal(
            req,
            "InternalError: InternalIterator is not declared",
        ));
    };
    let handlers = req.object_handlers(object.heap());
    let own = handlers.get_iterator(req, object, false)?;
    req.addref(&object.as_value());
    let (state, held) = match own {
        Some(own) => {
            let iter = wrap(req, Funcs::Class, object, Some(own))?;
            (
                Internal::Own {
                    iter,
                    rewound: false,
                },
                iter,
            )
        }
        None => (Internal::Elements { object, index: 0 }, object),
    };
    // `zend_internal_iterator_create`: `emalloc(sizeof(zend_internal_iterator))`
    // whole; and for a walk by index the iterator php's `get_iterator`
    // makes, which the class's handlers size, beside it.
    let by_index = matches!(state, Internal::Elements { .. });
    let iterator = req
        .new_object_sized(class, state, ZEND_INTERNAL_ITERATOR)
        .inspect_err(|_| req.release(held.as_value()))?;
    if by_index {
        req.lay_beside_sized(iterator, handlers.iterator_size())?;
    }
    Ok(iterator)
}

/// The element a walk by index stands at, through the walked object's
/// class's `iterator_element`.
fn internal_element(req: &mut Request, this: PhpObject) -> Result<Option<IteratorElement>, Thrown> {
    let Some(Internal::Elements { object, index }) = this.native::<Internal>(req) else {
        return Err(error(
            req,
            "The InternalIterator object has not been properly initialized",
        ));
    };
    let (object, index) = (*object, *index as usize);
    let table = req.object_handlers(object.heap());
    table.iterator_element(req, object, index)
}

/// php's `Error` for a `foreach` by reference over an iterator.
fn no_foreach_by_ref(req: &mut Request) -> Thrown {
    error(req, "An iterator cannot be used with foreach by reference")
}

/// Whether an iterator's element stands, the two owners it handed given
/// up.
fn element_stands(element: Option<IteratorElement>, req: &mut Request) -> bool {
    let Some((key, value)) = element else {
        return false;
    };
    key.give_up(req);
    value.give_up(req);
    true
}

/// An iterator's element's value, its key given up; `null` past the
/// end.
fn element_value(element: Option<IteratorElement>, req: &mut Request) -> Owner<Value> {
    let Some((key, value)) = element else {
        return Owner::null();
    };
    key.give_up(req);
    value
}

/// An iterator's element's key, its value given up; `null` past the
/// end.
fn element_key(element: Option<IteratorElement>, req: &mut Request) -> Owner<Value> {
    let Some((key, value)) = element else {
        return Owner::null();
    };
    value.give_up(req);
    key
}
