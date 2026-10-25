//! `common/common_enum.cpp`: `IntlIterator`, php's `Iterator` over a
//! `zend_object_iterator` the object holds, which its methods and a
//! `foreach` share. [`Walk`] is what it walks (the iterator's funcs), its
//! position the object's [`Cursor`]: the element last moved to and the
//! index php's engine counts. `IntlIterator_from_StringEnumeration` is
//! [`from_enumeration`]; `IntlPartsIterator` walks a break iterator's parts
//! through the same object.

use php_ext::abi::{ClassIterator, DtorObj, GcTable, ObjectHandlers, Refusals, This};
use php_ext::attr::php_class;
use php_ext::call::{lookup_class, new_object};
use php_ext::raising::{Thrown, fatal, throw, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString, Value};

use crate::error::{self, Carrier, IntlError};
use crate::ffi::Code;
use crate::ffi::enumeration::Enumeration;

/// What an `IntlIterator` walks: the funcs of its `zend_object_iterator`.
pub(crate) trait Walk: Send {
    /// `move_forward` after `invalidate_current`: the next element, one
    /// owner the caller's, or `None` at the end. `index` is the
    /// iterator's, which a parts iterator keyed by boundary sets.
    fn forward(
        &mut self,
        req: &mut Request,
        iterator: PhpObject,
        index: &mut i64,
    ) -> Result<Option<Owner<Value>>, Thrown>;

    /// `rewind` up to its `move_forward`: whether to move to the first
    /// element now.
    fn reset(&mut self, req: &mut Request, iterator: PhpObject) -> Result<bool, Thrown>;

    /// `get_current_key`, `None` where php has none and the key is the
    /// index.
    fn key(&self, index: i64) -> Option<i64> {
        let _ = index;
        None
    }

    /// `iterator->data`: the object the walk holds, which a parts
    /// iterator answers from `getBreakIterator`.
    fn data(&self) -> Option<Value> {
        None
    }

    /// `destroy_it`: what the walk holds of the heap given back.
    fn release(&mut self, req: &mut Request) {
        let _ = req;
    }

    /// Whether the cursor holds the object it is the cursor of, php's
    /// `wrapping_obj`: a `StringEnumeration`'s does; a parts iterator's
    /// is `UNDEF` since php 8.5.12, so that object is freed by its count.
    fn wraps_object(&self) -> bool {
        true
    }
}

/// The position over a [`Walk`]: `zoi_with_current`'s `current` and
/// `wrapping_obj`, and the iterator's `index`. `wrapping` is one owner of
/// the object that holds the cursor where the walk wraps it, as php's
/// `wrapping_obj` is, so an object nothing else owns is a cycle the
/// collector finds through [`Handlers::get_gc`] and counts.
pub(crate) struct Cursor {
    walk: Box<dyn Walk>,
    current: Option<Owner<Value>>,
    wrapping: Option<Owner<PhpObject>>,
    index: i64,
}

/// `IntlIterator_object`: the error and the iterator, `None` until a
/// factory lays one down.
#[derive(Default)]
pub(crate) struct IteratorObject {
    error: IntlError,
    cursor: Option<Box<Cursor>>,
}

impl Carrier for IteratorObject {
    fn error(&mut self) -> &mut IntlError {
        &mut self.error
    }
}

/// `IntlIterator_handlers`: no clone, the cursor given back with the
/// object, and `foreach` over the object's own iterator.
pub(crate) static HANDLERS: Handlers = Handlers;

/// The handlers of `IntlIterator` and `IntlPartsIterator`.
pub(crate) struct Handlers;

/// `class IntlIterator implements Iterator`: its methods and a `foreach`
/// move the one iterator the object holds; one `new` makes has none.
///
/// ```php
/// $x = new IntlIterator;
/// foreach (['current', 'key', 'next', 'rewind', 'valid'] as $m) {
///     try { $x->$m(); } catch (Error $e) { echo $m, ': ', get_class($e), ': ', $e->getMessage(), "\n"; }
/// }
/// try { foreach ($x as $v) {} } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { foreach ($x as &$v) {} } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { clone $x; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($x instanceof Iterator);
/// ```
/// ```expect
/// current: Error: Found unconstructed IntlIterator
/// key: Error: Found unconstructed IntlIterator
/// next: Error: Found unconstructed IntlIterator
/// rewind: Error: Found unconstructed IntlIterator
/// valid: Error: Found unconstructed IntlIterator
/// Exception: The IntlIterator is not properly constructed
/// Exception: Iteration by reference is not supported
/// Trying to clone an uncloneable object of class IntlIterator
/// bool(true)
/// ```
#[php_class(implements(Iterator), size = 80, beside(128), payload = IteratorObject, handlers = crate::iterator::HANDLERS)]
pub(crate) struct IntlIterator;

/// Core's `Iterator`, named for `implements`.
#[php_class(extern)]
pub(crate) struct Iterator;

#[php_class]
impl IntlIterator {
    #[php(tentative, before_args = error::reset)]
    fn current(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let object = this.object();
        fetch(req, object)?;
        let current = cursor(req, object)
            .and_then(|c| c.current.as_deref().copied())
            .unwrap_or_else(Value::null);
        Ok(current)
    }

    #[php(tentative, before_args = error::reset)]
    fn key(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let object = this.object();
        fetch(req, object)?;
        let key = cursor(req, object).map_or(0, |c| c.walk.key(c.index).unwrap_or(c.index));
        Ok(Value::int(key))
    }

    #[php(tentative, before_args = error::reset)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let object = this.object();
        fetch(req, object)?;
        with_cursor(req, object, |req, cursor| {
            forward(req, object, cursor)?;
            cursor.index += 1;
            Ok(())
        })
    }

    #[php(tentative, before_args = error::reset)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let object = this.object();
        fetch(req, object)?;
        with_cursor(req, object, |req, cursor| rewind(req, object, cursor))
    }

    #[php(tentative, before_args = error::reset)]
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let object = this.object();
        fetch(req, object)?;
        Ok(cursor(req, object).is_some_and(|c| c.current.is_some()))
    }
}

/// `INTLITERATOR_METHOD_FETCH_OBJECT`: the object's error reset, the
/// cursor, or php's `Error` for an object no factory made.
fn fetch(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    error::reset_object::<IteratorObject>(req, object);
    if cursor(req, object).is_none() {
        return Err(throw_error(
            req,
            "Error",
            "Found unconstructed IntlIterator",
        ));
    }
    Ok(())
}

/// The object's cursor, `None` for one no factory made.
fn cursor(req: &Request, object: PhpObject) -> Option<&Cursor> {
    object
        .native::<IteratorObject>(req)
        .and_then(|o| o.cursor.as_deref())
}

/// `f` over the object's cursor, taken out of the object for the call so
/// the walk can reach the request, and put back after.
fn with_cursor<T>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut Request, &mut Cursor) -> Result<T, Thrown>,
) -> Result<T, Thrown> {
    let taken = object
        .native_mut::<IteratorObject>(req)
        .and_then(|o| o.cursor.take());
    let Some(mut cursor) = taken else {
        return Err(throw_error(
            req,
            "Error",
            "Found unconstructed IntlIterator",
        ));
    };
    let answer = f(req, &mut cursor);
    match object.native_mut::<IteratorObject>(req) {
        Some(o) => o.cursor = Some(cursor),
        None => release(req, *cursor),
    }
    answer
}

/// `invalidate_current`, then the walk's `move_forward`.
fn forward(req: &mut Request, object: PhpObject, cursor: &mut Cursor) -> Result<(), Thrown> {
    if let Some(old) = cursor.current.take() {
        old.give_up(req);
    }
    cursor.current = cursor.walk.forward(req, object, &mut cursor.index)?;
    Ok(())
}

/// `funcs->rewind`: the walk reset, then moved to its first element.
fn rewind(req: &mut Request, object: PhpObject, cursor: &mut Cursor) -> Result<(), Thrown> {
    if let Some(old) = cursor.current.take() {
        old.give_up(req);
    }
    if cursor.walk.reset(req, object)? {
        forward(req, object, cursor)?;
    }
    Ok(())
}

/// `IntlIterator_objects_dtor`: the iterator released, which is
/// `zoi_with_current_dtor` since php 8.5.12: `invalidate_current`, then
/// `destroy_it`, then the wrapping object given up.
fn release(req: &mut Request, cursor: Cursor) {
    let Cursor {
        mut walk,
        current,
        wrapping,
        ..
    } = cursor;
    if let Some(old) = current {
        old.give_up(req);
    }
    walk.release(req);
    if let Some(wrapping) = wrapping {
        wrapping.give_up(req);
    }
}

/// The object's cursor taken and released, `ii->iterator = NULL`.
fn take_released(req: &mut Request, object: PhpObject) {
    let taken = object
        .native_mut::<IteratorObject>(req)
        .and_then(|o| o.cursor.take());
    if let Some(cursor) = taken {
        release(req, *cursor);
    }
}

/// A new object of `class` (`IntlIterator` or `IntlPartsIterator`)
/// walking `walk`, one owner, the caller's.
pub(crate) fn made(
    req: &mut Request,
    class: &[u8],
    walk: Box<dyn Walk>,
) -> Result<Owner<PhpObject>, Thrown> {
    let Some(id) = lookup_class(req, class)? else {
        return Err(fatal(req, "InternalError: IntlIterator is not linked"));
    };
    let object = new_object(req, id)?;
    let wrapping = walk.wraps_object().then(|| Owner::take(req, *object));
    let cursor = Box::new(Cursor {
        walk,
        current: None,
        wrapping,
        index: 0,
    });
    match object.native_mut::<IteratorObject>(req) {
        Some(o) => o.cursor = Some(cursor),
        None => release(req, *cursor),
    }
    Ok(object)
}

/// The walk an object of the class holds, for `IntlPartsIterator`'s
/// `getBreakIterator`: `iterator->data`.
pub(crate) fn data(req: &mut Request, object: PhpObject) -> Result<Option<Value>, Thrown> {
    fetch(req, object)?;
    Ok(cursor(req, object).and_then(|c| c.walk.data()))
}

/// `IntlIterator_from_StringEnumeration`: an `IntlIterator` over the
/// strings of `enumeration`.
pub(crate) fn from_enumeration(
    req: &mut Request,
    enumeration: Enumeration,
) -> Result<Owner<PhpObject>, Thrown> {
    made(req, b"IntlIterator", Box::new(Strings(enumeration)))
}

/// The walk of a `StringEnumeration`.
struct Strings(Enumeration);

impl Walk for Strings {
    /// `string_enum_current_move_forward`.
    fn forward(
        &mut self,
        req: &mut Request,
        iterator: PhpObject,
        _: &mut i64,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        let mut status = Code::ZERO;
        let next = self.0.next(&mut status);
        error::errors_set_code::<IteratorObject>(req, iterator, status);
        if status.is_failure() {
            let message = "Error fetching next iteration element";
            error::errors_set_custom_msg::<IteratorObject>(req, iterator, message)?;
            return Ok(None);
        }
        match next {
            Some(text) => Ok(Some(PhpString::new(req, &text)?.into_value())),
            None => Ok(None),
        }
    }

    /// `string_enum_rewind`.
    fn reset(&mut self, req: &mut Request, iterator: PhpObject) -> Result<bool, Thrown> {
        let mut status = Code::ZERO;
        self.0.reset(&mut status);
        error::errors_set_code::<IteratorObject>(req, iterator, status);
        if status.is_failure() {
            let message = "Error resetting enumeration";
            error::errors_set_custom_msg::<IteratorObject>(req, iterator, message)?;
            return Ok(false);
        }
        Ok(true)
    }
}

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `IntlIterator_objects_dtor`: the cursor released, which gives up
    /// the object's owner of itself, and no `__destruct` served, as php's
    /// slot serves none, so the request's end frees an object only its
    /// cursor owns.
    ///
    /// ```php
    /// class Sub extends IntlIterator { function __destruct() { echo "destruct\n"; } }
    /// $sub = new Sub;
    /// unset($sub);
    /// var_dump(get_class(IntlCalendar::getKeywordValuesForLocale('calendar', 'fa_IR', true)));
    /// echo "end\n";
    /// ```
    /// ```expect
    /// string(12) "IntlIterator"
    /// end
    /// ```
    fn dtor_obj(&self) -> DtorObj {
        DtorObj::Own(take_released)
    }

    /// `IntlIterator_object_get_gc` through `zoi_with_current_get_gc`:
    /// the wrapping object alone, which is the object itself, so one
    /// nothing else owns is garbage, counted once as php counts it; a
    /// parts iterator wraps nothing since php 8.5.12, so its count frees
    /// it and the collector finds none.
    ///
    /// ```php
    /// function cycle() {
    ///     $it = IntlCalendar::getKeywordValuesForLocale('calendar', 'fa_IR', true);
    ///     unset($it);
    ///     var_dump(gc_collect_cycles());
    /// }
    /// cycle();
    /// cycle();
    /// $parts = IntlBreakIterator::createWordInstance('en')->getPartsIterator();
    /// unset($parts);
    /// var_dump(gc_collect_cycles());
    /// ```
    /// ```expect
    /// int(1)
    /// int(1)
    /// int(0)
    /// ```
    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(wrapping) = cursor(req, object).and_then(|c| c.wrapping.as_ref()) {
            visit(wrapping.as_value());
        }
        GcTable::Properties
    }

    /// `IntlIterator_get_iterator`: the object's own iterator, which a
    /// `foreach` shares with the methods.
    fn get_iterator(
        &self,
        req: &mut Request,
        object: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        if by_ref {
            return Err(throw(
                req,
                "Exception",
                "Iteration by reference is not supported",
            ));
        }
        if cursor(req, object).is_none() {
            let message = "The IntlIterator is not properly constructed";
            return Err(throw(req, "Exception", message));
        }
        Ok(Some(Box::new(Shared)))
    }

    /// The `foreach` allocates nothing: it holds the object's iterator.
    fn iterator_size(&self) -> usize {
        0
    }
}

/// A `foreach` over the object's cursor, as php's engine drives the
/// `zend_object_iterator`: the index set to zero by the reset and
/// counted before each move.
struct Shared;

impl ClassIterator for Shared {
    fn rewind(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        with_cursor(req, object, |req, cursor| {
            cursor.index = 0;
            rewind(req, object, cursor)?;
            cursor.index = 0;
            Ok(())
        })
    }

    fn valid(&mut self, req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
        Ok(cursor(req, object).is_some_and(|c| c.current.is_some()))
    }

    fn current(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        let current = cursor(req, object)
            .and_then(|c| c.current.as_deref().copied())
            .unwrap_or_else(Value::null);
        Ok(Owner::take(req, current))
    }

    fn key(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        let key = cursor(req, object).map_or(0, |c| c.walk.key(c.index).unwrap_or(c.index));
        Ok(Owner::int(key))
    }

    fn move_forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        with_cursor(req, object, |req, cursor| {
            cursor.index += 1;
            forward(req, object, cursor)
        })
    }
}
