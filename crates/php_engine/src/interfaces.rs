//! `Zend/zend_interfaces.c` at 8.5.9: `Traversable`, `IteratorAggregate`,
//! `Iterator`, `Serializable` and `Countable` as
//! `zend_interfaces.stub.php` declares them, registered by
//! `zend_register_interfaces` before any other class, and the
//! `zend_user_iterator` a `foreach` over a `Traversable` object walks:
//! `getIterator()` asked until it answers an `Iterator`
//! (`zend_user_it_get_new_iterator`), then `rewind()` and `valid()` at
//! the reset and `next()`, `valid()`, `current()` and `key()` at each
//! fetch, in the order `zend_fe_fetch_object_helper` calls the iterator
//! functions. `ArrayAccess` and `Stringable` are `classes`'s.
//!
//! A class of the engine's or an extension's whose `get_iterator` php
//! writes in C (`ArrayIterator`, `SplFixedArray`) is walked the same way
//! here, through its methods: a native method does what php's iterator
//! function does, and a user override is served where php's
//! `zend_user_it_*` fallback serves it, so the order of the calls a
//! program sees is the same.

use php_unit::decl::{
    ClassDecl, ClassFlags, ClassKind, EnumBacking, FunctionDecl, MethodDecl, Name, Parameter, Type,
    Visibility,
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
        variadic: false,
        default: None,
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

/// `zend_user_it_get_new_iterator`
/// (`Z/Zend/zend_interfaces.c:179`): an `Iterator` is its own iterator
/// and an `IteratorAggregate` is asked with `getIterator()` until what
/// it answers is one, php's `Exception` naming the class that answered
/// something else. The iterator comes back with one owner, the
/// caller's.
pub(crate) fn iterator_of(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let iterator = req.class_named(b"iterator");
    let mut current = object;
    req.addref(&current.as_value());
    loop {
        if iterator.is_some_and(|i| req.instance_of(current.class(), i)) {
            return Ok(current);
        }
        let answered = crate::api::call::call_method(req, &current, b"getIterator", &[]);
        let class = current.class();
        req.release(current.as_value());
        let answered = answered?;
        match answered.as_object().copied() {
            Some(next) if is_traversable(req, next.class()) => current = next,
            _ => {
                req.release(answered);
                let name = req.class_name(class);
                return Err(throw(
                    req,
                    "Exception",
                    &format!(
                        "Objects returned by {name}::getIterator() must be traversable or implement interface Iterator"
                    ),
                ));
            }
        }
    }
}

/// `zend_user_it_valid`: `valid()` served and its return read as
/// `i_zend_is_true` reads it.
pub(crate) fn valid(req: &mut Request, iterator: PhpObject) -> Result<bool, Thrown> {
    let answered = crate::api::call::call_method(req, &iterator, b"valid", &[])?;
    let valid = crate::api::coerce::truthy(req, &answered);
    req.release(answered);
    Ok(valid)
}

/// `zend_user_it_rewind`, `zend_user_it_move_forward`,
/// `zend_user_it_get_current_data` and `zend_user_it_get_current_key`:
/// one method served, its return the caller's, one owner.
pub(crate) fn step(req: &mut Request, iterator: PhpObject, method: &[u8]) -> Result<Value, Thrown> {
    crate::api::call::call_method(req, &iterator, method, &[])
}

/// The name of php's `zend_iterator_class_entry`, `__iterator_wrapper`
/// (`Z/Zend/zend_iterators.c:59`), keyed with a leading `\0`: php never
/// adds the class to `EG(class_table)`, and the byte is the mark
/// `get_declared_classes()` skips a key by, so no program reaches it by
/// name or sees it in a listing.
pub(crate) const ITERATOR_WRAPPER: &[u8] = b"\0__iterator_wrapper";

/// `zend_user_iterator` (`Z/Zend/zend_interfaces.h:30`), php's
/// `zend_object_iterator` for a class walked by its methods: what a
/// `foreach` over a `Traversable` object holds in its loop variable from
/// the reset to `FE_FREE`, as `zend_fe_reset_iterator` puts `&iter->std`
/// there. It is the payload of an object of the `__iterator_wrapper`
/// class ([`ITERATOR_WRAPPER`]), so the loop owns it as it owns any
/// value: the release at `FE_FREE`, or at the unwind of the loop's live
/// range, gives back the `Iterator` it walks, and it takes the number
/// in the object store `zend_iterator_init` takes, which a `var_dump`
/// inside the body shows. The progress is `index`, never the loop variable's `u2` word.
///
/// A class php walks with an iterator of its own in C
/// (`zend_class_entry.get_iterator`, #735) is walked through its methods
/// here, so the functions are `zend_user_it_*`'s alone.
#[derive(Default)]
pub(crate) struct ObjectIterator {
    /// `it.data`: the `Iterator` whose methods are served, one owner;
    /// `None` only in an object nothing reset.
    data: Option<PhpObject>,
    /// `iter->index`: the fetches made since the reset, the first
    /// counting none; `None` from the reset to the first fetch, php's
    /// `-1`, which serves no `next()`.
    index: Option<u64>,
}

/// `iterator_object_handlers` (`Z/Zend/zend_iterators.c:28`):
/// `iter_wrapper_free` gives back the `Iterator` walked,
/// `iter_wrapper_dtor` serves nothing, `iter_wrapper_get_gc` names what
/// the iterator holds, and the object is uncloneable.
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

/// `zend_fe_reset_iterator` (`Z/Zend/zend_execute.c:5368`) over a
/// `Traversable` object, whose `get_iterator` is
/// `zend_user_it_get_new_iterator`: the `Iterator` found
/// ([`iterator_of`]), refused under `by_ref` as `zend_user_it_get_iterator`
/// refuses it, wrapped in a fresh [`ObjectIterator`], then `rewind()` and
/// `valid()`. The wrapper comes back with one owner, the loop's, and
/// whether `valid()` answered `false`, php's `is_empty`, which jumps
/// past the loop; on a throw nothing is held.
pub(crate) fn reset(
    req: &mut Request,
    object: PhpObject,
    by_ref: bool,
) -> Result<(PhpObject, bool), Thrown> {
    let data = iterator_of(req, object)?;
    if by_ref {
        req.release(data.as_value());
        return Err(throw(
            req,
            "Error",
            "An iterator cannot be used with foreach by reference",
        ));
    }
    let wrapper = match req.class_named(ITERATOR_WRAPPER) {
        Some(class) => req.new_object_with(
            class,
            ObjectIterator {
                data: Some(data),
                index: None,
            },
        ),
        None => Err(crate::api::raising::fatal(
            req,
            "InternalError: the iterator wrapper class is not declared",
        )),
    };
    let wrapper = match wrapper {
        Ok(wrapper) => wrapper,
        Err(thrown) => {
            req.release(data.as_value());
            return Err(thrown);
        }
    };
    let valid = step(req, data, b"rewind")
        .map(|rewound| req.release(rewound))
        .and_then(|()| valid(req, data));
    match valid {
        Ok(valid) => Ok((wrapper, !valid)),
        Err(thrown) => {
            req.release(wrapper.as_value());
            Err(thrown)
        }
    }
}

/// The iterator arm of `zend_fe_fetch_object_helper`
/// (`Z/Zend/zend_vm_def.h:7117`) and of `ZEND_FE_FETCH_RW`: the index
/// moved on, and past the first fetch `next()` and `valid()`; then
/// `current()`, and `key()` where the opline keeps it, its reference
/// followed as `zend_user_it_get_current_key` unwraps one. `None` is the
/// end, the fetch's jump past the loop. The values are the caller's,
/// one owner each.
pub(crate) fn fetch(
    req: &mut Request,
    wrapper: PhpObject,
    keyed: bool,
) -> Result<Option<(Value, Option<Value>)>, Thrown> {
    let Some(it) = wrapper.native_mut::<ObjectIterator>(req) else {
        return Ok(None);
    };
    let Some(data) = it.data else {
        return Ok(None);
    };
    let fetched = it.index;
    it.index = Some(fetched.map_or(0, |index| index.saturating_add(1)));
    if fetched.is_some() {
        let moved = step(req, data, b"next")?;
        req.release(moved);
        if !valid(req, data)? {
            return Ok(None);
        }
    }
    let current = step(req, data, b"current")?;
    if !keyed {
        return Ok(Some((current, None)));
    }
    match step(req, data, b"key") {
        Ok(key) => {
            let bare = key.deref();
            req.addref(&bare);
            req.release(key);
            Ok(Some((current, Some(bare))))
        }
        Err(thrown) => {
            req.release(current);
            Err(thrown)
        }
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
