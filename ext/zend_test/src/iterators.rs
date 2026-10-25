//! php's `ext/zend_test/iterators.c`: `ZendTest\Iterators\TraversableTest`,
//! a class walked in C by an iterator that writes each of its steps, for
//! the tests of how the engine drives an internal iterator.

use php_ext::abi::{ClassIterator, ExtensionBuilder, ObjectHandlers, This};
use php_ext::attr::php_class;
use php_ext::call::internal_iterator;
use php_ext::raising::{Thrown, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, Value};

use crate::throwables::Throwables;

/// The class, `zend_test_iterators_init`.
pub(crate) fn declared(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder.class::<TraversableTest>()
}

/// `final class ZendTest\Iterators\TraversableTest implements
/// \IteratorAggregate`, walked by [`Steps`]: the numbers 0 to 3, each
/// step written as it is taken.
///
/// ```php
/// foreach (new ZendTest\Iterators\TraversableTest as $k => $v) { echo "$k => $v\n"; }
/// $it = (new ZendTest\Iterators\TraversableTest)->getIterator();
/// var_dump($it->valid());
/// try { foreach (new ZendTest\Iterators\TraversableTest as &$v) {} } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// TraversableTest::rewind
/// TraversableTest::valid
/// TraversableTest::current
/// TraversableTest::key
/// 0 => 0
/// TraversableTest::next
/// TraversableTest::valid
/// TraversableTest::current
/// TraversableTest::key
/// 1 => 1
/// TraversableTest::next
/// TraversableTest::valid
/// TraversableTest::current
/// TraversableTest::key
/// 2 => 2
/// TraversableTest::next
/// TraversableTest::valid
/// TraversableTest::current
/// TraversableTest::key
/// 3 => 3
/// TraversableTest::next
/// TraversableTest::valid
/// TraversableTest::drop
/// TraversableTest::rewind
/// TraversableTest::valid
/// bool(true)
/// An iterator cannot be used with foreach by reference
/// TraversableTest::drop
/// ```
#[php_class(
    final,
    namespace(ZendTest::Iterators),
    implements(IteratorAggregate),
    handlers = crate::iterators::TRAVERSABLE_TEST
)]
pub(crate) struct TraversableTest;

/// Core's `IteratorAggregate`, named for `implements`.
#[php_class(extern)]
pub(crate) struct IteratorAggregate;

/// Core's `Iterator`, the return of `getIterator`.
#[php_class(extern)]
pub(crate) struct Iterator;

#[php_class]
impl TraversableTest {
    /// `__construct()`.
    fn __construct(this: This<Self>) {
        let _ = this;
    }

    /// `getIterator(): \Iterator`, `zend_create_internal_iterator_zval`.
    fn getIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<php_ext::abi::Instance<Iterator>>, Thrown> {
        Ok(internal_iterator(req, &this.object())?.into_instance())
    }
}

/// `TraversableTest`'s table: `std_object_handlers`, the class's
/// `get_iterator` answering [`Steps`].
pub(crate) struct TraversableTestHandlers;

pub(crate) static TRAVERSABLE_TEST: TraversableTestHandlers = TraversableTestHandlers;

impl ObjectHandlers for TraversableTestHandlers {
    /// `test_traversable_get_iterator`: refused by reference.
    fn get_iterator(
        &self,
        req: &mut Request,
        _object: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        if by_ref {
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
                "An iterator cannot be used with foreach by reference",
            ));
        }
        Ok(Some(Box::new(Steps(0))))
    }
}

/// `test_traversable_it`: where the walk stands, each step written
/// through the request as `DUMP` writes it.
struct Steps(i64);

impl ClassIterator for Steps {
    fn rewind(&mut self, req: &mut Request, _: PhpObject) -> Result<(), Thrown> {
        req.response_mut().write(b"TraversableTest::rewind\n");
        self.0 = 0;
        Ok(())
    }

    fn valid(&mut self, req: &mut Request, _: PhpObject) -> Result<bool, Thrown> {
        req.response_mut().write(b"TraversableTest::valid\n");
        Ok(self.0 < 4)
    }

    fn current(&mut self, req: &mut Request, _: PhpObject) -> Result<Owner<Value>, Thrown> {
        req.response_mut().write(b"TraversableTest::current\n");
        Ok(Owner::int(self.0))
    }

    fn key(&mut self, req: &mut Request, _: PhpObject) -> Result<Owner<Value>, Thrown> {
        req.response_mut().write(b"TraversableTest::key\n");
        Ok(Owner::int(self.0))
    }

    fn move_forward(&mut self, req: &mut Request, _: PhpObject) -> Result<(), Thrown> {
        req.response_mut().write(b"TraversableTest::next\n");
        self.0 += 1;
        Ok(())
    }

    fn release(&mut self, req: &mut Request, _: PhpObject) {
        req.response_mut().write(b"TraversableTest::drop\n");
    }
}
