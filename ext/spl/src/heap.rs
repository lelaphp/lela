//! `spl_heap.c`: `SplHeap`, `SplMinHeap`, `SplMaxHeap` and
//! `SplPriorityQueue` over php's `spl_ptr_heap`, the array-backed binary
//! heap whose sift up and sift down this file copies step for step, so
//! the order two equal elements come out in is php's. A comparison is
//! `zend_compare`, or the class's `compare()` where a user class
//! overrides it (`fptr_cmp`); one that throws leaves the heap marked
//! corrupted, and a heap is locked against writes while it sifts. One
//! body serves a heap and a queue where the two differ only by the
//! payload's kind, so a queue's method is the heap's `implementation =`.

use php_ext::abi::{CloneObj, ExtensionBuilder, ObjectHandlers, Std, This, True};
use php_ext::attr::php_class;
use php_ext::call::{call_method, clone_with};
use php_ext::coerce::{compare, to_long};
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, Value};

use crate::{Countable, Iterator};

/// `spl_heap_object` with its `spl_ptr_heap`: the elements, a value
/// each for a heap and a data and priority pair for a queue, the
/// queue's extract flags, and the heap's two state bits.
// The heap's kind, its two state bits and whether it was laid out, one
// each, as `spl_ptr_heap` and `spl_heap_object` keep them.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
pub(crate) struct HeapState {
    elements: Vec<(Value, Value)>,
    queue: bool,
    min: bool,
    flags: i64,
    corrupted: bool,
    locked: bool,
    ready: bool,
}

/// The payload: `spl_heap_object`'s fields, boxed, inside the `sizeof` php
/// allocates (design §5.13).
pub(crate) type Heap = Box<HeapState>;

/// `spl_handler_SplHeap` and `spl_handler_SplPriorityQueue`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// The four classes onto `builder`, in the order
/// `PHP_MINIT_FUNCTION(spl_heap)` registers them.
pub(crate) fn register(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder
        .class::<SplHeap>()
        .class::<SplMinHeap>()
        .class::<SplMaxHeap>()
        .class::<SplPriorityQueue>()
}

/// `abstract class SplHeap implements Iterator, Countable`: a walk is
/// the heap emptied from the top, `key()` counting down.
///
/// ```php
/// $h = new SplMinHeap;
/// foreach ([5, 1, 3, 1, 4] as $n) $h->insert($n);
/// echo count($h), " ", $h->count(), " ", $h->top(), "\n";
/// foreach ($h as $k => $v) echo "$k:$v;";
/// echo "\n", count($h), "\n";
/// $max = new SplMaxHeap; $max->insert('b'); $max->insert('c'); $max->insert('a');
/// echo $max->extract(), $max->extract(), $max->extract(), "\n";
/// $h->insert(9); $h->rewind(); var_dump($h->valid(), $h->current()); $h->next(); var_dump($h->valid());
/// $h->insert(2); $h->insert(1);
/// var_dump($h->isCorrupted()); $c = new SplMaxHeap; $c->__unserialize($h->__serialize()); var_dump($c->top());
/// ```
/// ```expect
/// 5 5 1
/// 4:1;3:1;2:3;1:4;0:5;
/// 0
/// cba
/// bool(true)
/// int(9)
/// bool(false)
/// bool(false)
/// int(2)
/// ```
///
/// The object is `zend_object_alloc(sizeof(spl_heap_object))`, with the
/// `spl_ptr_heap` and its first 64 elements `spl_ptr_heap_init` allocates
/// beside it, which `memory_get_usage()` counts (design §5.13):
///
/// ```php
/// foreach ([false, true] as $shown) foreach (['SplMinHeap', 'SplMaxHeap', 'SplPriorityQueue'] as $c) {
///     $r = new ReflectionClass($c);
///     $m = memory_get_usage();
///     $o = $r->newInstanceWithoutConstructor();
///     $used = memory_get_usage() - $m;
///     $o = null;
///     if ($shown) echo $c, ' ', $used, "\n";
/// }
/// ```
/// ```expect
/// SplMinHeap 1184
/// SplMaxHeap 1184
/// SplPriorityQueue 2208
/// ```
#[php_class(abstract, implements(Iterator, Countable), size = 88, beside(64, 1024), payload = Heap, handlers = HANDLERS)]
pub(crate) struct SplHeap;

#[php_class]
impl SplHeap {
    /// `extract(): mixed`: the top taken off.
    ///
    /// ```php
    /// $h = new SplMaxHeap;
    /// try { $h->extract(); } catch (RuntimeException $e) { echo $e->getMessage(), "\n"; }
    /// try { $h->top(); } catch (RuntimeException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Can't extract from an empty heap
    /// Can't peek at an empty heap
    /// ```
    #[php(tentative)]
    fn extract(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let this = this.object();
        consistent(req, this, true)?;
        let Some(top) = delete_top(req, this)? else {
            return Err(throw(
                req,
                "RuntimeException",
                "Can't extract from an empty heap",
            ));
        };
        let (queue, flags) = read(req, this, |h| (h.queue, h.flags))?;
        if !queue {
            return Ok(top.0);
        }
        let out = extracted(req, top, flags);
        release(req, top);
        out
    }

    /// `SplHeap::insert(mixed $value): true`: sifted up into place.
    ///
    /// ```php
    /// class Reverse extends SplHeap {
    ///     protected function compare($a, $b): int { echo "cmp($a,$b);"; return $b <=> $a; }
    /// }
    /// $h = new Reverse; $h->insert(2); $h->insert(1); $h->insert(3);
    /// echo "\n", implode(',', iterator_to_array($h, false)), "\n";
    /// ```
    /// ```expect
    /// cmp(2,1);cmp(1,3);
    /// cmp(3,2);cmp(3,2);1,2,3
    /// ```
    #[php(tentative)]
    fn insert(req: &mut Request, this: This<Self>, value: Value) -> Result<True, Thrown> {
        let this = this.object();
        consistent(req, this, true)?;
        let value = crate::copied(req, value);
        heap_insert(req, this, (value, Value::null()))?;
        Ok(True)
    }

    /// `top(): mixed`: the top, left in place.
    #[php(tentative)]
    fn top(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let this = this.object();
        consistent(req, this, false)?;
        let (queue, flags, top) = read(req, this, |h| {
            (h.queue, h.flags, h.elements.first().copied())
        })?;
        let Some(top) = top else {
            return Err(throw(
                req,
                "RuntimeException",
                "Can't peek at an empty heap",
            ));
        };
        if queue {
            extracted(req, top, flags)
        } else {
            Ok(crate::copied(req, top.0))
        }
    }

    /// `count(): int`.
    #[php(tentative)]
    fn count(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let n = read(req, this.object(), |h| h.elements.len())?;
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// `isEmpty(): bool`.
    ///
    /// ```php
    /// $h = new SplMinHeap; var_dump($h->isEmpty()); $h->insert(1); var_dump($h->isEmpty(), $h->key(), $h->current());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// int(0)
    /// int(1)
    /// ```
    #[php(tentative)]
    fn isEmpty(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        read(req, this.object(), |h| h.elements.is_empty())
    }

    /// `rewind(): void`: nothing, a heap walked is a heap emptied.
    #[php(tentative)]
    fn rewind(this: This<Self>) {
        let _ = this;
    }

    /// `current(): mixed`: the top, `null` for an empty heap.
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let (queue, flags, top) = read(req, this.object(), |h| {
            (h.queue, h.flags, h.elements.first().copied())
        })?;
        match top {
            None => Ok(Value::null()),
            Some(top) if queue => extracted(req, top, flags),
            Some(top) => Ok(crate::copied(req, top.0)),
        }
    }

    /// `key(): int`, `count - 1`.
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let n = read(req, this.object(), |h| h.elements.len())?;
        Ok(i64::try_from(n).unwrap_or(i64::MAX) - 1)
    }

    /// `next(): void`: the top taken off and dropped.
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = this.object();
        consistent(req, this, true)?;
        if let Some(top) = delete_top(req, this)? {
            release(req, top);
        }
        Ok(())
    }

    /// `valid(): bool`.
    #[php(tentative)]
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        read(req, this.object(), |h| !h.elements.is_empty())
    }

    /// `recoverFromCorruption(): true`.
    ///
    /// ```php
    /// class Bad extends SplMinHeap {
    ///     protected function compare($a, $b): int { throw new Exception("no"); }
    /// }
    /// $h = new Bad; $h->insert(1);
    /// try { $h->insert(2); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($h->isCorrupted(), count($h));
    /// try { $h->top(); } catch (RuntimeException $e) { echo $e->getMessage(), "\n"; }
    /// $h->recoverFromCorruption(); var_dump($h->isCorrupted());
    /// ```
    /// ```expect
    /// no
    /// bool(true)
    /// int(2)
    /// Heap is corrupted, heap properties are no longer ensured.
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn recoverFromCorruption(req: &mut Request, this: This<Self>) -> Result<True, Thrown> {
        with(req, this.object(), |h| h.corrupted = false)?;
        Ok(True)
    }

    #[php(tentative, protected)]
    fn compare(this: This<Self>, value1: Value, value2: Value) -> i64;

    /// `isCorrupted(): bool`; the witness is `recoverFromCorruption`'s.
    #[php(tentative)]
    fn isCorrupted(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        read(req, this.object(), |h| h.corrupted)
    }

    /// `__debugInfo(): array`.
    ///
    /// ```php
    /// $q = new SplPriorityQueue; $q->insert('a', 2);
    /// var_dump($q);
    /// $h = new SplMaxHeap; $h->insert(7);
    /// var_dump($h);
    /// ```
    /// ```expect
    /// object(SplPriorityQueue)#1 (3) {
    ///   ["flags":"SplPriorityQueue":private]=>
    ///   int(1)
    ///   ["isCorrupted":"SplPriorityQueue":private]=>
    ///   bool(false)
    ///   ["heap":"SplPriorityQueue":private]=>
    ///   array(1) {
    ///     [0]=>
    ///     array(2) {
    ///       ["data"]=>
    ///       string(1) "a"
    ///       ["priority"]=>
    ///       int(2)
    ///     }
    ///   }
    /// }
    /// object(SplMaxHeap)#2 (3) {
    ///   ["flags":"SplHeap":private]=>
    ///   int(0)
    ///   ["isCorrupted":"SplHeap":private]=>
    ///   bool(false)
    ///   ["heap":"SplHeap":private]=>
    ///   array(1) {
    ///     [0]=>
    ///     int(7)
    ///   }
    /// }
    /// ```
    #[php(tentative)]
    fn __debugInfo(req: &mut Request, this: This<Self>) -> Result<PhpArray, Thrown> {
        heap_debug_info(req, this.object())
    }

    /// `__serialize(): array`: the properties, then `flags` and
    /// `heap_elements` in heap order.
    ///
    /// ```php
    /// $h = new SplMinHeap; $h->insert(3); $h->insert(1);
    /// var_dump($h->__serialize());
    /// $c = new SplMinHeap; $c->__unserialize($h->__serialize()); var_dump($c->top());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   array(0) {
    ///   }
    ///   [1]=>
    ///   array(2) {
    ///     ["flags"]=>
    ///     int(0)
    ///     ["heap_elements"]=>
    ///     array(2) {
    ///       [0]=>
    ///       int(1)
    ///       [1]=>
    ///       int(3)
    ///     }
    ///   }
    /// }
    /// int(1)
    /// ```
    #[php(tentative)]
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<PhpArray, Thrown> {
        serialize(req, this.object())
    }

    /// `__unserialize(array $data): void`; the witness is `__serialize`'s.
    #[php(tentative)]
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        unserialize(req, this.object(), data)
    }
}

/// `class SplMinHeap extends SplHeap`: the smallest on top.
///
/// ```php
/// class Probe extends SplMinHeap { function probe($a, $b) { return $this->compare($a, $b); } }
/// $p = new Probe; var_dump($p->probe(1, 2), $p->probe(2, 1), $p->probe(3, 3));
/// ```
/// ```expect
/// int(1)
/// int(-1)
/// int(0)
/// ```
#[php_class(extends(SplHeap))]
pub(crate) struct SplMinHeap;

#[php_class]
impl SplMinHeap {
    /// `SplMinHeap::compare(mixed $value1, mixed $value2): int`,
    /// protected: the two the other way round.
    #[php(tentative, protected)]
    fn compare(
        req: &mut Request,
        this: This<Self>,
        value1: Value,
        value2: Value,
    ) -> Result<i64, Thrown> {
        let _ = this;
        Ok(compare(req, &value2, &value1)? as i64)
    }
}

/// `class SplMaxHeap extends SplHeap`: the largest on top, its
/// `compare()` `SplPriorityQueue::compare`'s body.
///
/// ```php
/// class Probe extends SplMaxHeap { function probe($a, $b) { return $this->compare($a, $b); } }
/// $p = new Probe; var_dump($p->probe(1, 2), $p->probe(2, 1));
/// ```
/// ```expect
/// int(-1)
/// int(1)
/// ```
#[php_class(extends(SplHeap))]
pub(crate) struct SplMaxHeap;

#[php_class]
impl SplMaxHeap {
    #[php(tentative, protected, implementation = SplPriorityQueue::compare)]
    fn compare(this: This<Self>, value1: Value, value2: Value) -> i64;
}

/// `class SplPriorityQueue implements Iterator, Countable`: each element
/// a data and a priority, what comes out chosen by the extract flags.
///
/// ```php
/// $q = new SplPriorityQueue;
/// foreach (['a' => 3, 'b' => 1, 'c' => 2] as $d => $p) $q->insert($d, $p);
/// echo count($q), $q->count(), " ", $q->isEmpty() ? "empty" : "full", " ", $q->top(), "\n";
/// foreach ($q as $k => $v) echo "$k:$v;";
/// echo "\n";
/// $q->insert('z', 0); $q->rewind(); var_dump($q->valid(), $q->key(), $q->current(), $q->extract(), $q->isCorrupted(), $q->recoverFromCorruption());
/// $q->insert('y', 0); $q->next(); var_dump($q->valid());
/// $c = new SplPriorityQueue; $q->insert('w', 5); $c->__unserialize($q->__serialize()); var_dump($c->__debugInfo()["\0SplPriorityQueue\0heap"][0]['data']);
/// var_dump($c->getExtractFlags());
/// ```
/// ```expect
/// 33 full a
/// 2:a;1:c;0:b;
/// bool(true)
/// int(0)
/// string(1) "z"
/// string(1) "z"
/// bool(false)
/// bool(true)
/// bool(false)
/// string(1) "w"
/// int(1)
/// ```
#[php_class(implements(Iterator, Countable), size = 88, beside(64, 2048), payload = Heap, handlers = HANDLERS)]
pub(crate) struct SplPriorityQueue;

#[php_class]
impl SplPriorityQueue {
    const EXTR_BOTH: i64 = 3;
    const EXTR_PRIORITY: i64 = 2;
    const EXTR_DATA: i64 = 1;

    /// `SplPriorityQueue::compare(mixed $priority1, mixed $priority2): int`.
    ///
    /// ```php
    /// $q = new SplPriorityQueue;
    /// var_dump($q->compare(1, 2), $q->compare(2, 1), (new SplMinHeap)->isEmpty());
    /// ```
    /// ```expect
    /// int(-1)
    /// int(1)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn compare(
        req: &mut Request,
        this: This<Self>,
        priority1: Value,
        priority2: Value,
    ) -> Result<i64, Thrown> {
        let _ = this;
        Ok(compare(req, &priority1, &priority2)? as i64)
    }

    /// `SplPriorityQueue::insert(mixed $value, mixed $priority): true`.
    ///
    /// ```php
    /// $q = new SplPriorityQueue;
    /// $q->insert('low', 1); $q->insert('high', 10); $q->insert('mid', 5); $q->insert('mid2', 5);
    /// $q->setExtractFlags(SplPriorityQueue::EXTR_BOTH);
    /// print_r($q->top());
    /// $q->setExtractFlags(SplPriorityQueue::EXTR_DATA);
    /// while ($q->valid()) { echo $q->extract(), ";"; }
    /// ```
    /// ```expect
    /// Array
    /// (
    ///     [data] => high
    ///     [priority] => 10
    /// )
    /// high;mid2;mid;low;
    /// ```
    #[php(tentative)]
    fn insert(
        req: &mut Request,
        this: This<Self>,
        value: Value,
        priority: Value,
    ) -> Result<True, Thrown> {
        let this = this.object();
        consistent(req, this, true)?;
        let data = crate::copied(req, value);
        let priority = crate::copied(req, priority);
        heap_insert(req, this, (data, priority))?;
        Ok(True)
    }

    /// `setExtractFlags(int $flags): int`.
    ///
    /// ```php
    /// $q = new SplPriorityQueue; $q->insert('x', 1);
    /// var_dump($q->setExtractFlags(SplPriorityQueue::EXTR_PRIORITY), $q->getExtractFlags(), $q->current());
    /// try { $q->setExtractFlags(0); } catch (RuntimeException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(2)
    /// int(2)
    /// int(1)
    /// Must specify at least one extract flag
    /// ```
    #[php(tentative)]
    fn setExtractFlags(req: &mut Request, this: This<Self>, flags: i64) -> Result<i64, Thrown> {
        let flags = flags & Self::EXTR_BOTH;
        if flags == 0 {
            return Err(throw(
                req,
                "RuntimeException",
                "Must specify at least one extract flag",
            ));
        }
        with(req, this.object(), |h| h.flags = flags)?;
        Ok(flags)
    }

    #[php(tentative, implementation = SplHeap::top)]
    fn top(this: This<Self>) -> Value;
    #[php(tentative, implementation = SplHeap::extract)]
    fn extract(this: This<Self>) -> Value;
    #[php(tentative, implementation = SplHeap::count)]
    fn count(this: This<Self>) -> i64;
    #[php(tentative, implementation = SplHeap::isEmpty)]
    fn isEmpty(this: This<Self>) -> bool;
    #[php(tentative, implementation = SplHeap::rewind)]
    fn rewind(this: This<Self>);
    #[php(tentative, implementation = SplHeap::current)]
    fn current(this: This<Self>) -> Value;
    #[php(tentative, implementation = SplHeap::key)]
    fn key(this: This<Self>) -> i64;
    #[php(tentative, implementation = SplHeap::next)]
    fn next(this: This<Self>);
    #[php(tentative, implementation = SplHeap::valid)]
    fn valid(this: This<Self>) -> bool;
    #[php(tentative, implementation = SplHeap::recoverFromCorruption)]
    fn recoverFromCorruption(this: This<Self>) -> True;
    #[php(tentative, implementation = SplHeap::isCorrupted)]
    fn isCorrupted(this: This<Self>) -> bool;

    /// `getExtractFlags(): int`; the witness is `setExtractFlags`'s.
    #[php(tentative)]
    fn getExtractFlags(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        read(req, this.object(), |h| h.flags)
    }

    #[php(tentative, implementation = SplHeap::__debugInfo)]
    fn __debugInfo(this: This<Self>) -> PhpArray;
    #[php(tentative, implementation = SplHeap::__serialize)]
    fn __serialize(this: This<Self>) -> PhpArray;
    #[php(tentative, implementation = SplHeap::__unserialize)]
    fn __unserialize(this: This<Self>, data: PhpArray);
}

/// The payload, laid out on first use as `spl_heap_object_new_ex` lays
/// it out for the class: a queue under `SplPriorityQueue`, a min-heap
/// under `SplMinHeap`, a max-heap otherwise.
fn state(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    let ready = object.native::<Heap>(req).map(|h| h.ready);
    match ready {
        Some(true) => Ok(()),
        Some(false) => {
            let queue = crate::is_a(req, object.class(), b"SplPriorityQueue");
            let min = crate::is_a(req, object.class(), b"SplMinHeap");
            if let Some(h) = object.native_mut::<Heap>(req) {
                h.queue = queue;
                h.min = min;
                h.flags = if queue {
                    SplPriorityQueue::EXTR_DATA
                } else {
                    0
                };
                h.ready = true;
            }
            Ok(())
        }
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: an SPL heap method served on an object without its payload",
        )),
    }
}

/// The payload of `object` changed by `change`.
fn with<R>(
    req: &mut Request,
    object: PhpObject,
    change: impl FnOnce(&mut Heap) -> R,
) -> Result<R, Thrown> {
    state_ready(req, object)?;
    if let Some(h) = object.native_mut::<Heap>(req) {
        return Ok(change(h));
    }
    Err(php_ext::raising::fatal(
        req,
        "InternalError: an SPL heap method served on an object without its payload",
    ))
}

fn state_ready(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    if object.native::<Heap>(req).is_some_and(|h| h.ready) {
        return Ok(());
    }
    state(req, object)
}

/// A read of the payload.
fn read<R>(
    req: &mut Request,
    object: PhpObject,
    look: impl FnOnce(&Heap) -> R,
) -> Result<R, Thrown> {
    state_ready(req, object)?;
    if let Some(h) = object.native::<Heap>(req) {
        return Ok(look(h));
    }
    Err(php_ext::raising::fatal(
        req,
        "InternalError: an SPL heap method served on an object without its payload",
    ))
}

/// `spl_heap_consistency_validations`.
fn consistent(req: &mut Request, object: PhpObject, write: bool) -> Result<(), Thrown> {
    let (corrupted, locked) = read(req, object, |h| (h.corrupted, h.locked))?;
    if corrupted {
        return Err(throw(
            req,
            "RuntimeException",
            "Heap is corrupted, heap properties are no longer ensured.",
        ));
    }
    if write && locked {
        return Err(throw(
            req,
            "RuntimeException",
            "Heap cannot be changed when it is already being modified.",
        ));
    }
    Ok(())
}

/// `heap->cmp`: `spl_ptr_heap_zval_max_cmp`, its min twin or
/// `spl_ptr_pqueue_elem_cmp`, over the element for a heap and the
/// priority for a queue, through the class's `compare()` where a user
/// class overrides it. What a comparison throws is kept in `pending`
/// and every comparison after it is equal, as `EG(exception)` makes
/// them.
fn cmp(
    req: &mut Request,
    object: PhpObject,
    left: &(Value, Value),
    right: &(Value, Value),
    pending: &mut Option<Thrown>,
) -> i64 {
    if pending.is_some() {
        return 0;
    }
    let Ok((queue, min)) = read(req, object, |h| (h.queue, h.min)) else {
        return 0;
    };
    let (first, second) = if queue {
        (left.1, right.1)
    } else {
        (left.0, right.0)
    };
    let answer = if overridden_compare(req, object) {
        call_method(req, &object, b"compare", &[first, second]).and_then(|answered| {
            let n = to_long(req, &answered);
            req.release(answered);
            n
        })
    } else if min && !queue {
        compare(req, &second, &first).map(|o| o as i64)
    } else {
        compare(req, &first, &second).map(|o| o as i64)
    };
    match answer {
        Ok(n) => n.signum(),
        Err(thrown) => {
            *pending = Some(thrown);
            0
        }
    }
}

/// `fptr_cmp`: the class's `compare()` is a user method.
fn overridden_compare(req: &mut Request, object: PhpObject) -> bool {
    Std.get_method(req, object, b"compare")
        .ok()
        .flatten()
        .and_then(|method| req.function_decl(method))
        .is_some_and(|decl| !decl.file.0.is_empty())
}

fn element(req: &Request, object: PhpObject, at: usize) -> (Value, Value) {
    object
        .native::<Heap>(req)
        .and_then(|h| h.elements.get(at).copied())
        .unwrap_or_default()
}

fn put(
    req: &mut Request,
    object: PhpObject,
    at: usize,
    value: (Value, Value),
) -> Result<(), Thrown> {
    with(req, object, |h| {
        if let Some(slot) = h.elements.get_mut(at) {
            *slot = value;
        }
    })
}

/// `spl_ptr_heap_insert`: the hole sifted up from the end while the
/// parent compares below the new element, the element written where the
/// hole stops; a comparison that threw marks the heap corrupted.
fn heap_insert(req: &mut Request, object: PhpObject, elem: (Value, Value)) -> Result<(), Thrown> {
    let count = with(req, object, |h| {
        h.locked = true;
        h.elements.push((Value::null(), Value::null()));
        h.elements.len() - 1
    })?;
    let mut pending = None;
    let mut pos = count;
    while pos > 0 {
        let parent = element(req, object, (pos - 1) / 2);
        if cmp(req, object, &parent, &elem, &mut pending) >= 0 {
            break;
        }
        put(req, object, pos, parent)?;
        pos = (pos - 1) / 2;
    }
    let threw = pending.is_some();
    with(req, object, |h| {
        h.locked = false;
        if threw {
            h.corrupted = true;
        }
    })?;
    put(req, object, pos, elem)?;
    pending.map_or(Ok(()), Err)
}

/// `spl_ptr_heap_delete_top`: the top handed back, the last element
/// sifted down from the root past each larger child.
fn delete_top(req: &mut Request, object: PhpObject) -> Result<Option<(Value, Value)>, Thrown> {
    let count = read(req, object, |h| h.elements.len())?;
    if count == 0 {
        return Ok(None);
    }
    let limit = (count - 1) / 2;
    with(req, object, |h| h.locked = true)?;
    let top = element(req, object, 0);
    let count = count - 1;
    let bottom = element(req, object, count);
    let mut pending = None;
    let mut parent = 0;
    while parent < limit {
        let mut child = parent * 2 + 1;
        if child != count {
            let right = element(req, object, child + 1);
            let left = element(req, object, child);
            if cmp(req, object, &right, &left, &mut pending) > 0 {
                child += 1;
            }
        }
        let at_child = element(req, object, child);
        if cmp(req, object, &bottom, &at_child, &mut pending) < 0 {
            put(req, object, parent, at_child)?;
        } else {
            break;
        }
        parent = child;
    }
    let threw = pending.is_some();
    with(req, object, |h| {
        h.locked = false;
        if threw {
            h.corrupted = true;
        }
        if parent != count
            && let Some(slot) = h.elements.get_mut(parent)
        {
            *slot = bottom;
        }
        h.elements.truncate(count);
    })?;
    match pending {
        Some(thrown) => {
            release(req, top);
            Err(thrown)
        }
        None => Ok(Some(top)),
    }
}

fn release(req: &mut Request, elem: (Value, Value)) {
    req.release(elem.0);
    req.release(elem.1);
}

/// `spl_pqueue_extract_helper`: the data, the priority or both, one
/// more owner each.
fn extracted(req: &mut Request, elem: (Value, Value), flags: i64) -> Result<Value, Thrown> {
    if flags & SplPriorityQueue::EXTR_BOTH == SplPriorityQueue::EXTR_BOTH {
        let mut out = PhpArray::new(req)?;
        let data = PhpString::new(req, b"data")?;
        out.set(req, Key::Str(data), crate::copied(req, elem.0))?;
        let priority = PhpString::new(req, b"priority")?;
        out.set(req, Key::Str(priority), crate::copied(req, elem.1))?;
        return Ok(Value::array(out));
    }
    if flags & SplPriorityQueue::EXTR_PRIORITY != 0 {
        return Ok(crate::copied(req, elem.1));
    }
    Ok(crate::copied(req, elem.0))
}

impl ObjectHandlers for Handlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let elements = object
            .native_mut::<Heap>(req)
            .map(|h| std::mem::take(&mut h.elements));
        for elem in elements.unwrap_or_default() {
            release(req, elem);
        }
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(h) = object.native::<Heap>(req) {
            for (a, b) in &h.elements {
                visit(*a);
                if h.queue {
                    visit(*b);
                }
            }
        }
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_heap)
    }

    fn count_elements(&self, req: &mut Request, object: PhpObject) -> Result<Option<i64>, Thrown> {
        if crate::overridden(req, object, b"count") {
            let answered = call_method(req, &object, b"count", &[])?;
            let n = to_long(req, &answered)?;
            req.release(answered);
            return Ok(Some(n));
        }
        let n = read(req, object, |h| h.elements.len())?;
        Ok(Some(i64::try_from(n).unwrap_or(i64::MAX)))
    }
}

/// `spl_heap_object_clone`: the elements with one more owner each.
fn clone_heap(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    state_ready(req, object)?;
    let copy = read(req, object, |h| HeapState {
        elements: h.elements.clone(),
        queue: h.queue,
        min: h.min,
        flags: h.flags,
        corrupted: h.corrupted,
        locked: h.locked,
        ready: true,
    })?;
    for (a, b) in &copy.elements {
        req.addref(a);
        req.addref(b);
    }
    clone_with(req, &object, Box::new(copy))
}

/// `spl_heap_object_get_debug_info`: the properties, then `flags`,
/// `isCorrupted` and `heap` under the private names of `SplHeap` or
/// `SplPriorityQueue`.
fn heap_debug_info(req: &mut Request, object: PhpObject) -> Result<PhpArray, Thrown> {
    let (queue, flags, corrupted, elements) = read(req, object, |h| {
        (h.queue, h.flags, h.corrupted, h.elements.clone())
    })?;
    let mut out = PhpArray::new(req)?;
    for (key, value) in crate::std_properties(req, object)? {
        req.addref(&value);
        out.set(req, key, value)?;
    }
    let base: &[u8] = if queue {
        b"SplPriorityQueue"
    } else {
        b"SplHeap"
    };
    let mangled = |field: &[u8]| [b"\0".as_slice(), base, b"\0", field].concat();
    let name = PhpString::new(req, &mangled(b"flags"))?;
    out.set(req, Key::Str(name), Value::int(flags))?;
    let name = PhpString::new(req, &mangled(b"isCorrupted"))?;
    out.set(req, Key::Str(name), Value::bool(corrupted))?;
    let mut heap = PhpArray::new(req)?;
    for elem in elements {
        let value = if queue {
            extracted(req, elem, SplPriorityQueue::EXTR_BOTH)?
        } else {
            crate::copied(req, elem.0)
        };
        heap.append(req, value)?;
    }
    let name = PhpString::new(req, &mangled(b"heap"))?;
    out.set(req, Key::Str(name), Value::array(heap))?;
    Ok(out)
}

/// The array `__serialize` answers for a heap or a queue.
fn serialize(req: &mut Request, this: PhpObject) -> Result<PhpArray, Thrown> {
    consistent(req, this, false)?;
    let (queue, flags, elements) = read(req, this, |h| (h.queue, h.flags, h.elements.clone()))?;
    let mut out = PhpArray::new(req)?;
    let props = crate::members(req, this)?;
    out.append(req, Value::array(props))?;
    let mut state = PhpArray::new(req)?;
    let key = PhpString::new(req, b"flags")?;
    state.set(req, Key::Str(key), Value::int(flags))?;
    let mut heap = PhpArray::new(req)?;
    for elem in elements {
        let value = if queue {
            extracted(req, elem, SplPriorityQueue::EXTR_BOTH)?
        } else {
            crate::copied(req, elem.0)
        };
        heap.append(req, value)?;
    }
    let key = PhpString::new(req, b"heap_elements")?;
    state.set(req, Key::Str(key), Value::array(heap))?;
    out.append(req, Value::array(state))?;
    Ok(out)
}

/// A heap or a queue restored from what `__serialize` answered.
fn unserialize(req: &mut Request, this: PhpObject, data: PhpArray) -> Result<(), Thrown> {
    let queue = read(req, this, |h| h.queue)?;
    if !queue {
        consistent(req, this, true)?;
    }
    let invalid = |req: &mut Request| {
        let class = String::from_utf8_lossy(&crate::class_name(req, this.class())).into_owned();
        Err(throw(
            req,
            "Exception",
            &format!("Invalid serialization data for {class} object"),
        ))
    };
    let props = data.get(&Key::Int(0)).map(|v| v.deref());
    let state = data.get(&Key::Int(1)).map(|v| v.deref());
    let (Some(props), Some(state)) = (
        props.and_then(|p| p.as_array().copied()),
        state.and_then(|s| s.as_array().copied()),
    ) else {
        return invalid(req);
    };
    if data.len() != 2 {
        return invalid(req);
    }
    crate::load_members(req, this, &Value::array(props))?;
    let flags = state.find(b"flags").and_then(|f| f.deref().as_int());
    let elements = state
        .find(b"heap_elements")
        .and_then(|e| e.deref().as_array().copied());
    let (Some(mut flags), Some(elements)) = (flags, elements) else {
        return invalid(req);
    };
    if queue {
        flags &= SplPriorityQueue::EXTR_BOTH;
        if flags == 0 {
            return invalid(req);
        }
    } else if flags != 0 {
        return invalid(req);
    }
    with(req, this, |h| h.flags = flags)?;
    for (_, value) in elements.snapshot() {
        let elem = if queue {
            let Some(pair) = value.as_array().copied().filter(|p| p.len() == 2) else {
                return invalid(req);
            };
            let (Some(d), Some(p)) = (pair.find(b"data"), pair.find(b"priority")) else {
                return invalid(req);
            };
            (crate::copied(req, d), crate::copied(req, p))
        } else {
            (crate::copied(req, value), Value::null())
        };
        heap_insert(req, this, elem)?;
    }
    Ok(())
}
