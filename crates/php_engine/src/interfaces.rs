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

use php_unit::decl::{
    ClassDecl, ClassFlags, ClassKind, EnumBacking, FunctionDecl, MethodDecl, Name, Parameter,
    ParameterDefault, Type, Visibility,
};

use crate::api::abi::{Entry, NativeClassDecl, ObjectHandlers};
use crate::api::raising::{Thrown, throw};
use crate::api::request::Request;
use crate::api::values::{ClassId, PhpObject, Value};

/// The five interfaces in `zend_register_interfaces`' order.
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
pub(crate) fn classes() -> Vec<NativeClassDecl> {
    let tentative = |name: &[u8], parameters: Vec<Parameter>, returns: Option<Type>| MethodDecl {
        tentative_return: returns.is_some(),
        ..abstract_method(name, parameters, returns)
    };
    let data = Parameter {
        name: Name(b"data".as_slice().into()),
        ty: Some(Type::String),
        by_ref: false,
        prefer_ref: false,
        promoted: false,
        variadic: false,
        default: ParameterDefault::None,
        attributes: Vec::new(),
    };
    [
        interface(b"Traversable", &[], Vec::new()),
        interface(
            b"IteratorAggregate",
            &[b"Traversable"],
            vec![tentative(
                b"getIterator",
                Vec::new(),
                Some(Type::Named(Name(b"Traversable".as_slice().into()))),
            )],
        ),
        interface(
            b"Iterator",
            &[b"Traversable"],
            vec![
                tentative(b"current", Vec::new(), Some(Type::Mixed)),
                tentative(b"next", Vec::new(), Some(Type::Void)),
                tentative(b"key", Vec::new(), Some(Type::Mixed)),
                tentative(b"valid", Vec::new(), Some(Type::Bool)),
                tentative(b"rewind", Vec::new(), Some(Type::Void)),
            ],
        ),
        interface(
            b"Serializable",
            &[],
            vec![
                abstract_method(b"serialize", Vec::new(), None),
                abstract_method(b"unserialize", vec![data], None),
            ],
        ),
        interface(
            b"Countable",
            &[],
            vec![tentative(b"count", Vec::new(), Some(Type::Int))],
        ),
    ]
    .into_iter()
    .map(|decl| (decl, Vec::new(), None, None))
    .chain(std::iter::once(internal_iterator_class()))
    .chain(std::iter::once(iterator_wrapper_class()))
    .collect()
}

fn interface(name: &[u8], extends: &[&[u8]], methods: Vec<MethodDecl>) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind: ClassKind::Interface,
        flags: ClassFlags::default(),
        parent: None,
        interfaces: extends.iter().map(|i| Name((*i).into())).collect(),
        traits: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        properties: Vec::new(),
        methods,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    }
}

fn abstract_method(name: &[u8], parameters: Vec<Parameter>, returns: Option<Type>) -> MethodDecl {
    MethodDecl {
        function: FunctionDecl {
            name: Name(name.into()),
            parameters,
            return_type: returns,
            returns_by_ref: false,
            is_generator: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
            frameless: Vec::new(),
        },
        visibility: Visibility::Public,
        is_instance: true,
        is_abstract: true,
        is_final: false,
        tentative_return: false,
    }
}

/// Whether `class` is `Traversable`, php's `ce->get_iterator` being set:
/// what sends a `foreach` over the object to the iterator and not to
/// its properties.
pub(crate) fn is_traversable(req: &Request, class: ClassId) -> bool {
    req.class_named(b"traversable")
        .is_some_and(|traversable| req.instance_of(class, traversable))
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
}

impl Funcs {
    /// `funcs->rewind`: `zend_user_it_rewind`, or
    /// `zend_generator_iterator_rewind`, which runs the generator to its
    /// first `yield` and refuses a generator already past it.
    fn rewind(self, req: &mut Request, data: PhpObject) -> Result<(), Thrown> {
        match self {
            Funcs::User => {
                let answered = crate::api::call::call_method(req, &data, b"rewind", &[])?;
                req.release(answered);
                Ok(())
            }
            Funcs::Generator => crate::generator::iterator_rewind(req, data),
        }
    }

    /// `funcs->valid`, `SUCCESS` as `true`: `zend_user_it_valid`'s
    /// `valid()` read as `i_zend_is_true` reads it, or whether the
    /// generator still has its frame.
    fn valid(self, req: &mut Request, data: PhpObject) -> Result<bool, Thrown> {
        match self {
            Funcs::User => {
                let answered = crate::api::call::call_method(req, &data, b"valid", &[])?;
                let valid = crate::api::coerce::truthy(req, &answered);
                req.release(answered);
                Ok(valid)
            }
            Funcs::Generator => crate::generator::iterator_valid(req, data),
        }
    }

    /// `funcs->move_forward`: `next()` served, or the generator resumed.
    fn move_forward(self, req: &mut Request, data: PhpObject) -> Result<(), Thrown> {
        match self {
            Funcs::User => {
                let answered = crate::api::call::call_method(req, &data, b"next", &[])?;
                req.release(answered);
                Ok(())
            }
            Funcs::Generator => crate::generator::iterator_move_forward(req, data),
        }
    }

    /// `funcs->get_current_data`, one owner the caller's: what
    /// `current()` answered, or the value the generator yielded with a
    /// reference followed. `by_ref` is `FE_FETCH_RW`'s
    /// `ZVAL_MAKE_REF` over the zval php's function points at: the
    /// generator's own value made a reference in place, the cell handed
    /// out; `current()`'s answer is no zval anyone else holds, and is
    /// handed out as it is for the fetch to wrap.
    fn current(self, req: &mut Request, data: PhpObject, by_ref: bool) -> Result<Value, Thrown> {
        match self {
            Funcs::User => crate::api::call::call_method(req, &data, b"current", &[]),
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
        }
    }

    /// `funcs->get_current_key`, one owner the caller's:
    /// `zend_user_it_get_current_key`'s `key()` with a reference
    /// unwrapped, or the generator's key, `null` where it has none.
    fn key(self, req: &mut Request, data: PhpObject) -> Result<Value, Thrown> {
        match self {
            Funcs::User => {
                let answered = crate::api::call::call_method(req, &data, b"key", &[])?;
                let bare = answered.deref();
                req.addref(&bare);
                req.release(answered);
                Ok(bare)
            }
            Funcs::Generator => crate::generator::iterator_key(req, data),
        }
    }
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
/// `Generator`. A class php walks with an iterator of its own in C
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
        if let Some(held) = object
            .native_mut::<ObjectIterator>(req)
            .and_then(|it| it.data.take())
        {
            req.release(held.as_value());
        }
    }

    fn dtor_obj(&self, req: &mut Request, object: PhpObject) {
        let _ = (req, object);
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(held) = object.native::<ObjectIterator>(req).and_then(|it| it.data) {
            visit(held.as_value());
        }
    }

    fn clone_obj(&self) -> Option<crate::api::abi::CloneObj> {
        None
    }

    fn clone_obj_with(&self) -> Option<crate::api::abi::CloneObjWith> {
        None
    }
}

/// `zend_register_iterator_wrapper`'s class: final, of no member, its
/// objects carrying an [`ObjectIterator`].
fn iterator_wrapper_class() -> NativeClassDecl {
    let mut decl = interface(ITERATOR_WRAPPER, &[], Vec::new());
    decl.kind = ClassKind::Class;
    decl.flags.is_final = true;
    (
        decl,
        Vec::new(),
        Some(&WRAPPER),
        Some(crate::api::abi::new_payload::<ObjectIterator>()),
    )
}

/// `zend_iterator_unwrap` (`Z/Zend/zend_iterators.c:98`) as the test it
/// is in `FE_FETCH_*`: whether `object` is an iterator's wrapper, php's
/// comparison of the object's handler table, here of its payload's type.
pub(crate) fn is_iterator_wrapper(req: &Request, object: PhpObject) -> bool {
    object.native::<ObjectIterator>(req).is_some()
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
/// object itself. On a throw nothing is held.
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
/// ```
/// ```expect
/// a=1 b=2 10,20
/// Objects returned by Me::getIterator() must be traversable or implement interface Iterator
/// ```
pub(crate) fn get_iterator(
    req: &mut Request,
    object: PhpObject,
    by_ref: bool,
) -> Result<PhpObject, Thrown> {
    let (funcs, data) = iterated(req, object, by_ref)?;
    let wrapper = match req.class_named(ITERATOR_WRAPPER) {
        Some(class) => req.new_object_with(
            class,
            ObjectIterator {
                funcs,
                data: Some(data),
                index: None,
            },
        ),
        None => Err(crate::api::raising::fatal(
            req,
            "InternalError: the iterator wrapper class is not declared",
        )),
    };
    wrapper.inspect_err(|_| req.release(data.as_value()))
}

/// The table [`get_iterator`] chooses for `object` and the object it
/// walks, one owner the caller's.
fn iterated(
    req: &mut Request,
    object: PhpObject,
    by_ref: bool,
) -> Result<(Funcs, PhpObject), Thrown> {
    if crate::generator::is_generator(req, object) {
        crate::generator::get_iterator(req, object, by_ref)?;
        req.addref(&object.as_value());
        return Ok((Funcs::Generator, object));
    }
    if req
        .class_named(b"iterator")
        .is_some_and(|iterator| req.instance_of(object.class(), iterator))
    {
        if by_ref {
            return Err(throw(
                req,
                "Error",
                "An iterator cannot be used with foreach by reference",
            ));
        }
        req.addref(&object.as_value());
        return Ok((Funcs::User, object));
    }
    let answered = crate::api::call::call_method(req, &object, b"getIterator", &[])?;
    match answered.deref().as_object().copied() {
        Some(inner) if inner != object && is_traversable(req, inner.class()) => {
            let walked = iterated(req, inner, by_ref);
            req.release(answered);
            walked
        }
        _ => {
            req.release(answered);
            let name = req.class_name(object.class());
            Err(throw(
                req,
                "Exception",
                &format!(
                    "Objects returned by {name}::getIterator() must be traversable or implement interface Iterator"
                ),
            ))
        }
    }
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
    let valid = funcs
        .rewind(req, data)
        .and_then(|()| funcs.valid(req, data));
    match valid {
        Ok(valid) => Ok((wrapper, !valid)),
        Err(thrown) => {
            req.release(wrapper.as_value());
            Err(thrown)
        }
    }
}

/// The table and the object walked of the iterator `wrapper` holds, the
/// index moved on as `++iter->index` moves it, and whether this is a
/// fetch past the first, which moves the walk forward first.
fn step(req: &mut Request, wrapper: PhpObject) -> Option<(Funcs, PhpObject, bool)> {
    let it = wrapper.native_mut::<ObjectIterator>(req)?;
    let data = it.data?;
    let fetched = it.index;
    it.index = Some(fetched.map_or(0, |index| index.saturating_add(1)));
    Some((it.funcs, data, fetched.is_some()))
}

/// The table and the object walked of the iterator `wrapper` holds.
fn walked(req: &Request, wrapper: PhpObject) -> Option<(Funcs, PhpObject)> {
    let it = wrapper.native::<ObjectIterator>(req)?;
    it.data.map(|data| (it.funcs, data))
}

/// The iterator arm of `zend_fe_fetch_object_helper`
/// (`Z/Zend/zend_vm_def.h:7117`) and of `ZEND_FE_FETCH_RW`: past the
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
    if moves {
        funcs.move_forward(req, data)?;
        if !funcs.valid(req, data)? {
            return Ok(None);
        }
    }
    let current = funcs.current(req, data, by_ref)?;
    if !keyed {
        return Ok(Some((current, None)));
    }
    match funcs.key(req, data) {
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
    if moves {
        funcs.move_forward(req, data)?;
    }
    if !funcs.valid(req, data)? {
        return Ok(None);
    }
    let current = funcs.current(req, data, false)?;
    match funcs.key(req, data) {
        Ok(key) => Ok(Some((current, key))),
        Err(thrown) => {
            req.release(current);
            Err(thrown)
        }
    }
}

/// `iter->funcs->rewind` over the iterator `wrapper` holds, what
/// `YIELD_FROM` does to the iterator it takes before it stores it.
pub(crate) fn rewind(req: &mut Request, wrapper: PhpObject) -> Result<(), Thrown> {
    match walked(req, wrapper) {
        Some((funcs, data)) => funcs.rewind(req, data),
        None => Ok(()),
    }
}

/// `zend_internal_iterator`: the object walked, how many steps past its
/// rewind the walk stands, and whether it was rewound, php's
/// `rewind_called`.
#[derive(Default)]
struct Internal {
    object: Option<PhpObject>,
    index: usize,
}

/// `zend_internal_iterator_handlers`: `free_obj` gives back the object
/// walked, `get_gc` names it, and the class is uncloneable.
struct InternalHandlers;

static INTERNAL: InternalHandlers = InternalHandlers;

impl ObjectHandlers for InternalHandlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        if let Some(held) = object
            .native_mut::<Internal>(req)
            .and_then(|it| it.object.take())
        {
            req.release(held.as_value());
        }
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(held) = object.native::<Internal>(req).and_then(|it| it.object) {
            visit(held.as_value());
        }
    }

    fn clone_obj(&self) -> Option<crate::api::abi::CloneObj> {
        None
    }
}

/// `final class InternalIterator implements Iterator` as
/// `zend_interfaces.stub.php` declares it, its constructor private.
///
/// ```php
/// $f = SplFixedArray::fromArray(['a', 'b']);
/// $it = $f->getIterator();
/// var_dump(get_class($it), $it instanceof Iterator);
/// foreach ($it as $k => $v) echo "$k=$v ";
/// $it->rewind(); $it->next(); var_dump($it->key(), $it->current(), $it->valid());
/// $it->next(); var_dump($it->valid(), $it->current());
/// ```
/// ```expect
/// string(16) "InternalIterator"
/// bool(true)
/// 0=a 1=b int(1)
/// string(1) "b"
/// bool(true)
/// bool(false)
/// NULL
/// ```
fn internal_iterator_class() -> NativeClassDecl {
    let method = |name: &[u8], returns: Option<Type>| MethodDecl {
        is_abstract: false,
        ..abstract_method(name, Vec::new(), returns)
    };
    let mut decl = interface(b"InternalIterator", &[], Vec::new());
    decl.kind = ClassKind::Class;
    decl.flags.is_final = true;
    decl.interfaces = vec![Name(b"Iterator".as_slice().into())];
    let methods: Vec<(MethodDecl, Entry)> = vec![
        (
            MethodDecl {
                visibility: Visibility::Private,
                ..method(b"__construct", None)
            },
            internal_construct as Entry,
        ),
        (method(b"current", Some(Type::Mixed)), internal_current),
        (method(b"key", Some(Type::Mixed)), internal_key),
        (method(b"next", Some(Type::Void)), internal_next),
        (method(b"valid", Some(Type::Bool)), internal_valid),
        (method(b"rewind", Some(Type::Void)), internal_rewind),
    ];
    (
        decl,
        methods,
        Some(&INTERNAL),
        Some(crate::api::abi::new_payload::<Internal>()),
    )
}

/// `zend_create_internal_iterator_zval`.
pub(crate) fn internal_iterator(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let Some(class) = req.class_named(b"internaliterator") else {
        return Err(crate::api::raising::fatal(
            req,
            "InternalError: InternalIterator is not declared",
        ));
    };
    let iterator = req.new_object(class)?;
    req.addref(&object.as_value());
    if let Some(it) = iterator.native_mut::<Internal>(req) {
        it.object = Some(object);
    }
    Ok(iterator)
}

/// `InternalIterator::__construct`: php's `Error`.
fn internal_construct(req: &mut Request) -> Result<Value, Thrown> {
    Err(throw(
        req,
        "Error",
        "Cannot manually construct InternalIterator",
    ))
}

/// The element the iterator stands at, through the walked object's
/// class's `iterator_element`.
fn internal_element(req: &mut Request) -> Result<Option<(Value, Value)>, Thrown> {
    let Some(this) = req.this() else {
        return Ok(None);
    };
    let Some((object, index)) = this
        .native::<Internal>(req)
        .and_then(|it| it.object.map(|o| (o, it.index)))
    else {
        return Err(throw(
            req,
            "Error",
            "The InternalIterator object has not been properly initialized",
        ));
    };
    let table = req.object_handlers(object.heap());
    table.iterator_element(req, object, index)
}

/// `InternalIterator::current(): mixed`.
fn internal_current(req: &mut Request) -> Result<Value, Thrown> {
    Ok(match internal_element(req)? {
        Some((key, value)) => {
            req.release(key);
            value
        }
        None => Value::null(),
    })
}

/// `InternalIterator::key(): mixed`.
fn internal_key(req: &mut Request) -> Result<Value, Thrown> {
    Ok(match internal_element(req)? {
        Some((key, value)) => {
            req.release(value);
            key
        }
        None => Value::null(),
    })
}

/// `InternalIterator::next(): void`.
// An entry's shape, which cannot fail here.
#[allow(clippy::unnecessary_wraps)]
fn internal_next(req: &mut Request) -> Result<Value, Thrown> {
    if let Some(this) = req.this()
        && let Some(it) = this.native_mut::<Internal>(req)
    {
        it.index += 1;
    }
    Ok(Value::null())
}

/// `InternalIterator::valid(): bool`.
fn internal_valid(req: &mut Request) -> Result<Value, Thrown> {
    Ok(Value::bool(match internal_element(req)? {
        Some((key, value)) => {
            req.release(key);
            req.release(value);
            true
        }
        None => false,
    }))
}

/// `InternalIterator::rewind(): void`.
// An entry's shape, which cannot fail here.
#[allow(clippy::unnecessary_wraps)]
fn internal_rewind(req: &mut Request) -> Result<Value, Thrown> {
    if let Some(this) = req.this()
        && let Some(it) = this.native_mut::<Internal>(req)
    {
        it.index = 0;
    }
    Ok(Value::null())
}
