//! The iterator a `Traversable` hands out, php's `zend_object_iterator`
//! for the two kinds of `get_iterator` this engine has: a `Generator`'s
//! (`zend_generator_iterator_functions`) and a user class's that
//! implements `Iterator` (`zend_user_iterator`, `Z/Zend/zend_interfaces.c`),
//! an `IteratorAggregate` handing out its `getIterator()`'s. What `foreach`
//! and `yield from` walk an object with where the class is `Traversable`,
//! and a plain object's properties where it is not.
//!
//! php allocates a separate iterator object and wraps it in the zval; an
//! iterator here is the object iterated itself, one owner the walker
//! holds, and php's `iter->index` stands beside it where the walker
//! keeps its position (`Z_FE_POS` for `foreach`, the delegated values
//! for `yield from`). Which functions serve it is read off its class.

use crate::api::raising::{Thrown, throw};
use crate::api::request::Request;
use crate::api::values::{PhpObject, Value};

/// Which `zend_object_iterator_funcs` serve an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Generator,
    User,
}

/// Whether `object`'s class implements the interface named `lowered`.
fn implements(req: &Request, object: PhpObject, lowered: &[u8]) -> bool {
    req.class_named(lowered)
        .is_some_and(|interface| req.instance_of(object.class(), interface))
}

/// The functions an iterator is served by; none for an object whose
/// class has no `get_iterator`.
fn kind(req: &Request, object: PhpObject) -> Option<Kind> {
    if crate::generator::is_generator(req, object) {
        return Some(Kind::Generator);
    }
    implements(req, object, b"iterator").then_some(Kind::User)
}

/// Whether `ce->get_iterator` is set for `object`'s class: a
/// `Generator`, an `Iterator` or an `IteratorAggregate`.
pub(crate) fn is_traversable(req: &Request, object: PhpObject) -> bool {
    kind(req, object).is_some() || implements(req, object, b"iteratoraggregate")
}

/// `ce->get_iterator(ce, object, 0)`: the object to walk, one owner the
/// caller holds, or none where the class has no `get_iterator`. An
/// `IteratorAggregate` is `zend_user_it_get_new_iterator`: its
/// `getIterator()` asked, and what that returned walked in turn.
///
/// ```php
/// class A implements IteratorAggregate { function getIterator(): Iterator { return new I; } }
/// class I implements Iterator {
///     private $i = 0;
///     function rewind(): void { echo "rewind "; $this->i = 0; }
///     function valid(): bool { echo "valid "; return $this->i < 2; }
///     function current(): mixed { echo "current "; return $this->i * 10; }
///     function key(): mixed { echo "key "; return "k$this->i"; }
///     function next(): void { echo "next "; $this->i++; }
/// }
/// foreach (new A as $k => $v) echo "[$k=$v] ";
/// echo "|\n";
/// class B implements IteratorAggregate { function getIterator(): Traversable { yield 1; yield 2; } }
/// foreach (new B as $k => $v) echo "$k=$v ";
/// ```
/// ```expect
/// rewind valid current key [k0=0] next valid current key [k1=10] next valid |
/// 0=1 1=2
/// ```
pub(crate) fn get_iterator(
    req: &mut Request,
    object: PhpObject,
    by_ref: bool,
) -> Result<Option<PhpObject>, Thrown> {
    match kind(req, object) {
        Some(Kind::Generator) => {
            crate::generator::get_iterator(req, object, by_ref)?;
            req.addref(&object.as_value());
            return Ok(Some(object));
        }
        Some(Kind::User) => {
            if by_ref {
                // `zend_user_it_get_iterator`.
                return Err(throw(
                    req,
                    "Error",
                    "An iterator cannot be used with foreach by reference",
                ));
            }
            req.addref(&object.as_value());
            return Ok(Some(object));
        }
        None => {}
    }
    if !implements(req, object, b"iteratoraggregate") {
        return Ok(None);
    }
    let returned = call(req, object, b"getiterator")?;
    let inner = returned.deref().as_object().copied();
    let Some(inner) = inner.filter(|inner| is_traversable(req, *inner)) else {
        req.release(returned);
        let name = req.class_name(object.class());
        return Err(throw(
            req,
            "Exception",
            &format!(
                "Objects returned by {name}::getIterator() must be traversable or implement interface Iterator"
            ),
        ));
    };
    let walked = get_iterator(req, inner, by_ref);
    req.release(returned);
    walked
}

/// A method of the user iterator called with no argument,
/// `zend_user_it_invoke_method`; what it returned is the caller's.
fn call(req: &mut Request, object: PhpObject, lowered: &[u8]) -> Result<Value, Thrown> {
    let Some(function) = req.method_named(object.class(), lowered) else {
        let name = req.class_name(object.class());
        return Err(throw(
            req,
            "Error",
            &format!(
                "Call to undefined method {name}::{}()",
                String::from_utf8_lossy(lowered)
            ),
        ));
    };
    req.serve_method(function, Some(object), object.class(), &[], &[])
}

/// `funcs->rewind`.
pub(crate) fn rewind(req: &mut Request, iterator: PhpObject) -> Result<(), Thrown> {
    match kind(req, iterator) {
        Some(Kind::Generator) => crate::generator::iterator_rewind(req, iterator),
        Some(Kind::User) => {
            let returned = call(req, iterator, b"rewind")?;
            req.release(returned);
            Ok(())
        }
        None => Ok(()),
    }
}

/// `funcs->valid`: `SUCCESS` is `true`.
pub(crate) fn valid(req: &mut Request, iterator: PhpObject) -> Result<bool, Thrown> {
    match kind(req, iterator) {
        Some(Kind::Generator) => crate::generator::iterator_valid(req, iterator),
        Some(Kind::User) => {
            let returned = call(req, iterator, b"valid")?;
            let more = crate::api::coerce::truthy(req, &returned);
            req.release(returned);
            Ok(more)
        }
        None => Ok(false),
    }
}

/// `funcs->get_current_data`: the current value, one owner.
pub(crate) fn current(req: &mut Request, iterator: PhpObject) -> Result<Value, Thrown> {
    match kind(req, iterator) {
        Some(Kind::Generator) => {
            let value = crate::generator::iterator_current(req, iterator)?.deref();
            let value = value.undef_as_null();
            req.addref(&value);
            Ok(value)
        }
        Some(Kind::User) => {
            let returned = call(req, iterator, b"current")?;
            let value = returned.deref();
            req.addref(&value);
            req.release(returned);
            Ok(value)
        }
        None => Ok(Value::null()),
    }
}

/// `funcs->get_current_data` for `FE_FETCH_RW`: the zval the generator
/// holds, made a reference in place where it is none, one more owner of
/// the cell. Only a generator's iterator is walked by reference.
pub(crate) fn current_ref(
    req: &mut Request,
    iterator: PhpObject,
) -> Result<crate::api::values::PhpRef, Thrown> {
    crate::generator::iterator_current_ref(req, iterator)
}

/// `funcs->get_current_key`: the current key, one owner. `index` is
/// php's `iter->index`, which a class with no key function would read.
pub(crate) fn key(req: &mut Request, iterator: PhpObject, index: i64) -> Result<Value, Thrown> {
    match kind(req, iterator) {
        Some(Kind::Generator) => crate::generator::iterator_key(req, iterator),
        Some(Kind::User) => {
            let returned = call(req, iterator, b"key")?;
            let value = returned.deref().undef_as_null();
            req.addref(&value);
            req.release(returned);
            Ok(value)
        }
        None => Ok(Value::int(index)),
    }
}

/// `funcs->move_forward`.
pub(crate) fn move_forward(req: &mut Request, iterator: PhpObject) -> Result<(), Thrown> {
    match kind(req, iterator) {
        Some(Kind::Generator) => crate::generator::iterator_move_forward(req, iterator),
        Some(Kind::User) => {
            let returned = call(req, iterator, b"next")?;
            req.release(returned);
            Ok(())
        }
        None => Ok(()),
    }
}
