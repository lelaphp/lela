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

use crate::api::abi::NativeClassDecl;
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
