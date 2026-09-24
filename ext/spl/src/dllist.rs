//! `spl_dllist.c`: `SplDoublyLinkedList`, `SplQueue` and `SplStack`
//! over php's `spl_ptr_llist`. The nodes stand in an arena, each with
//! its neighbours by index, so a node the traverse pointer stands on
//! keeps its links as php's refcounted element does when it is popped,
//! shifted or unset under the pointer: a node taken out has its links
//! cut on the side php cuts them, and the walk ends there as php's
//! does. `IT_MODE_LIFO` walks from the tail, `IT_MODE_DELETE` takes
//! each element off as the walk passes it, and `SplQueue` and
//! `SplStack` freeze the direction (`SPL_DLLIST_IT_FIX`).

use php_ext::abi::{CloneObj, ObjectHandlers, PropertyPurpose, Std};
use php_ext::call::{call_method, clone_with};
use php_ext::coerce::to_long;
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, Value};

use crate::Entry;
use crate::stub::{self, Declared};

const IT_DELETE: i64 = 1;
const IT_LIFO: i64 = 2;
const IT_MASK: i64 = 3;
const IT_FIX: i64 = 4;

/// One `spl_ptr_llist_element`: its data, `None` once taken out, and
/// its neighbours.
#[derive(Clone, Copy, Default)]
struct Node {
    data: Option<Value>,
    prev: Option<usize>,
    next: Option<usize>,
}

/// `spl_dllist_object`: the list, the traverse pointer and position,
/// and the flags.
#[derive(Default)]
pub(crate) struct SplDllist {
    nodes: Vec<Node>,
    head: Option<usize>,
    tail: Option<usize>,
    count: usize,
    pointer: Option<usize>,
    position: i64,
    flags: i64,
    ready: bool,
}

impl SplDllist {
    fn push(&mut self, value: Value) {
        let at = self.nodes.len();
        self.nodes.push(Node {
            data: Some(value),
            prev: self.tail,
            next: None,
        });
        match self.tail {
            Some(tail) => self.nodes[tail].next = Some(at),
            None => self.head = Some(at),
        }
        self.tail = Some(at);
        self.count += 1;
    }

    fn unshift(&mut self, value: Value) {
        let at = self.nodes.len();
        self.nodes.push(Node {
            data: Some(value),
            prev: None,
            next: self.head,
        });
        match self.head {
            Some(head) => self.nodes[head].prev = Some(at),
            None => self.tail = Some(at),
        }
        self.head = Some(at);
        self.count += 1;
    }

    /// `spl_ptr_llist_pop`: the tail's data, its `prev` cut.
    fn pop(&mut self) -> Option<Value> {
        let tail = self.tail?;
        let prev = self.nodes[tail].prev;
        match prev {
            Some(prev) => self.nodes[prev].next = None,
            None => self.head = None,
        }
        self.tail = prev;
        self.count -= 1;
        self.nodes[tail].prev = None;
        self.nodes[tail].data.take()
    }

    /// `spl_ptr_llist_shift`: the head's data, its `next` cut.
    fn shift(&mut self) -> Option<Value> {
        let head = self.head?;
        let next = self.nodes[head].next;
        match next {
            Some(next) => self.nodes[next].prev = None,
            None => self.tail = None,
        }
        self.head = next;
        self.count -= 1;
        self.nodes[head].next = None;
        self.nodes[head].data.take()
    }

    /// `spl_ptr_llist_offset`: the node `offset` from the head, or from
    /// the tail under LIFO.
    fn offset(&self, offset: i64, backward: bool) -> Option<usize> {
        let mut current = if backward { self.tail } else { self.head };
        let mut pos = 0;
        while let Some(at) = current {
            if pos >= offset {
                break;
            }
            pos += 1;
            current = if backward {
                self.nodes[at].prev
            } else {
                self.nodes[at].next
            };
        }
        current
    }

    /// The data from head to tail.
    fn values(&self) -> Vec<Value> {
        let mut out = Vec::with_capacity(self.count);
        let mut current = self.head;
        while let Some(at) = current {
            if let Some(v) = self.nodes[at].data {
                out.push(v);
            }
            current = self.nodes[at].next;
        }
        out
    }
}

/// `spl_handler_SplDoublyLinkedList`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

const STUB: &str = "
class SplDoublyLinkedList implements Iterator, Countable, ArrayAccess, Serializable {
    public const int IT_MODE_LIFO = 2;
    public const int IT_MODE_FIFO = 0;
    public const int IT_MODE_DELETE = 1;
    public const int IT_MODE_KEEP = 0;
    /** @tentative-return-type */ public function add(int $index, mixed $value): void {}
    /** @tentative-return-type */ public function pop(): mixed {}
    /** @tentative-return-type */ public function shift(): mixed {}
    /** @tentative-return-type */ public function push(mixed $value): void {}
    /** @tentative-return-type */ public function unshift(mixed $value): void {}
    /** @tentative-return-type */ public function top(): mixed {}
    /** @tentative-return-type */ public function bottom(): mixed {}
    /** @tentative-return-type */ public function __debugInfo(): array {}
    /** @tentative-return-type */ public function count(): int {}
    /** @tentative-return-type */ public function isEmpty(): bool {}
    /** @tentative-return-type */ public function setIteratorMode(int $mode): int {}
    /** @tentative-return-type */ public function getIteratorMode(): int {}
    /** @tentative-return-type */ public function offsetExists($index): bool {}
    /** @tentative-return-type */ public function offsetGet($index): mixed {}
    /** @tentative-return-type */ public function offsetSet($index, mixed $value): void {}
    /** @tentative-return-type */ public function offsetUnset($index): void {}
    /** @tentative-return-type */ public function rewind(): void {}
    /** @tentative-return-type */ public function current(): mixed {}
    /** @tentative-return-type */ public function key(): int {}
    /** @tentative-return-type */ public function prev(): void {}
    /** @tentative-return-type */ public function next(): void {}
    /** @tentative-return-type */ public function valid(): bool {}
    /** @tentative-return-type */ public function unserialize(string $data): void {}
    /** @tentative-return-type */ public function serialize(): string {}
    /** @tentative-return-type */ public function __serialize(): array {}
    /** @tentative-return-type */ public function __unserialize(array $data): void {}
}
class SplQueue extends SplDoublyLinkedList {
    /** @tentative-return-type */ public function enqueue(mixed $value): void {}
    /** @tentative-return-type */ public function dequeue(): mixed {}
}
class SplStack extends SplDoublyLinkedList {
}
";

/// The three classes, each method with its entry; `enqueue` and
/// `dequeue` are php's aliases of `push` and `shift`.
///
/// ```php
/// $s = new SplStack; $s->push(1); $s->push(2); $s[] = 3;
/// foreach ($s as $k => $v) echo "$k:$v;";
/// echo "\n", $s->top(), " ", $s->pop(), " ", count($s), "\n";
/// $q = new SplQueue; $q->enqueue('a'); $q->enqueue('b');
/// echo $q->dequeue(), count($q), "\n";
/// try { $q->setIteratorMode(SplDoublyLinkedList::IT_MODE_LIFO); } catch (RuntimeException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 2:3;1:2;0:1;
/// 3 3 2
/// a1
/// Iterators' LIFO/FIFO modes for SplStack/SplQueue objects are frozen
/// ```
pub(crate) fn classes() -> Vec<Declared> {
    let base = "SplDoublyLinkedList";
    let rows: [(&str, Entry); 28] = [
        ("add", add),
        ("pop", pop),
        ("shift", shift),
        ("push", push),
        ("unshift", unshift),
        ("top", top),
        ("bottom", bottom),
        ("__debugInfo", debug_info),
        ("count", count),
        ("isEmpty", is_empty),
        ("setIteratorMode", set_iterator_mode),
        ("getIteratorMode", get_iterator_mode),
        ("offsetExists", offset_exists),
        ("offsetGet", offset_get),
        ("offsetSet", offset_set),
        ("offsetUnset", offset_unset),
        ("rewind", rewind),
        ("current", current),
        ("key", key),
        ("prev", prev),
        ("next", next),
        ("valid", valid),
        ("unserialize", unserialize),
        ("serialize", serialize),
        ("__serialize", magic_serialize),
        ("__unserialize", magic_unserialize),
        ("SplQueue::enqueue", push),
        ("SplQueue::dequeue", shift),
    ];
    let names: Vec<(String, Entry)> = rows
        .iter()
        .map(|(k, e)| {
            if k.contains("::") {
                ((*k).to_owned(), *e)
            } else {
                (format!("{base}::{k}"), *e)
            }
        })
        .collect();
    let entries: Vec<(&str, Entry)> = names.iter().map(|(k, e)| (k.as_str(), *e)).collect();
    stub::classes(STUB, &entries)
}

/// The payload of `object`, the flags laid down on first use as
/// `spl_dllist_object_new_ex` lays them for the class.
fn ready(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    match object.native::<SplDllist>(req).map(|l| l.ready) {
        Some(true) => Ok(()),
        Some(false) => {
            let flags = if crate::is_a(req, object.class(), b"SplStack") {
                IT_FIX | IT_LIFO
            } else if crate::is_a(req, object.class(), b"SplQueue") {
                IT_FIX
            } else {
                0
            };
            if let Some(l) = object.native_mut::<SplDllist>(req) {
                l.flags = flags;
                l.ready = true;
            }
            Ok(())
        }
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: an SPL list method served on an object without its payload",
        )),
    }
}

fn with<R>(
    req: &mut Request,
    object: PhpObject,
    change: impl FnOnce(&mut SplDllist) -> R,
) -> Result<R, Thrown> {
    ready(req, object)?;
    match object.native_mut::<SplDllist>(req) {
        Some(l) => Ok(change(l)),
        None => Ok(change(&mut SplDllist::default())),
    }
}

fn this(req: &mut Request) -> Result<PhpObject, Thrown> {
    let this = crate::this(req)?;
    ready(req, this)?;
    Ok(this)
}

/// `zend_argument_error(spl_ce_OutOfRangeException, 1, ...)`.
fn out_of_range(req: &mut Request, tail: &str) -> Thrown {
    php_ext::raising::argument_error(req, "OutOfRangeException", 1, tail)
}

impl ObjectHandlers for Handlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let values = object
            .native_mut::<SplDllist>(req)
            .map(|l| {
                let values = l.values();
                l.nodes.clear();
                l.head = None;
                l.tail = None;
                values
            })
            .unwrap_or_default();
        for value in values {
            req.release(value);
        }
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(l) = object.native::<SplDllist>(req) {
            for value in l.values() {
                visit(value);
            }
        }
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_list)
    }

    fn count_elements(&self, req: &mut Request, object: PhpObject) -> Result<Option<i64>, Thrown> {
        if crate::overridden(req, object, b"count") {
            let answered = call_method(req, &object, b"count", &[])?;
            let n = to_long(req, &answered)?;
            req.release(answered);
            return Ok(Some(n));
        }
        let n = with(req, object, |l| l.count)?;
        Ok(Some(i64::try_from(n).unwrap_or(i64::MAX)))
    }
}

/// `spl_dllist_object_clone`: the list copied, one more owner of each
/// element, the pointer at the head.
fn clone_list(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let (values, flags) = with(req, object, |l| (l.values(), l.flags))?;
    let mut copy = SplDllist {
        flags,
        ready: true,
        ..SplDllist::default()
    };
    for value in values {
        req.addref(&value);
        copy.push(value);
    }
    copy.pointer = copy.head;
    clone_with(req, &object, copy)
}

/// `spl_dllist_object_get_debug_info`: the properties, then `flags` and
/// `dllist` under `SplDoublyLinkedList`'s private names.
fn list_debug_info(req: &mut Request, object: PhpObject) -> Result<PhpArray, Thrown> {
    let (flags, values) = with(req, object, |l| (l.flags, l.values()))?;
    let mut out = PhpArray::new(req)?;
    for (name, value) in object.props_for(req, PropertyPurpose::Debug)? {
        req.addref(&value);
        out.set(req, Key::Str(name), value)?;
    }
    let name = PhpString::new(req, b"\0SplDoublyLinkedList\0flags")?;
    out.set(req, Key::Str(name), Value::int(flags))?;
    let mut list = PhpArray::new(req)?;
    for value in values {
        list.append(req, crate::copied(req, value))?;
    }
    let name = PhpString::new(req, b"\0SplDoublyLinkedList\0dllist")?;
    out.set(req, Key::Str(name), Value::array(list))?;
    Ok(out)
}

/// `add(int $index, mixed $value): void`: before the element at the
/// index, or at the end for the count.
///
/// ```php
/// $l = new SplDoublyLinkedList; $l->push('a'); $l->push('c');
/// $l->add(1, 'b'); $l->add(3, 'd');
/// echo implode(',', iterator_to_array($l)), "\n";
/// try { $l->add(9, 'x'); } catch (OutOfRangeException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// a,b,c,d
/// SplDoublyLinkedList::add(): Argument #1 ($index) is out of range
/// ```
fn add(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 2, 2)?;
    let this = this(req)?;
    let index = crate::long(req, 1)?;
    let value = crate::value(req, 2);
    let count = with(req, this, |l| l.count)?;
    if index < 0 || index > i64::try_from(count).unwrap_or(i64::MAX) {
        return Err(out_of_range(req, "is out of range"));
    }
    let value = crate::copied(req, value);
    with(req, this, |l| {
        if index == i64::try_from(l.count).unwrap_or(i64::MAX) {
            l.push(value);
            return;
        }
        let Some(element) = l.offset(index, l.flags & IT_LIFO != 0) else {
            return;
        };
        let at = l.nodes.len();
        let prev = l.nodes[element].prev;
        l.nodes.push(Node {
            data: Some(value),
            prev,
            next: Some(element),
        });
        match prev {
            None => l.head = Some(at),
            Some(prev) => l.nodes[prev].next = Some(at),
        }
        l.nodes[element].prev = Some(at);
        l.count += 1;
    })?;
    Ok(Value::null())
}

/// `pop(): mixed`.
///
/// ```php
/// $l = new SplDoublyLinkedList; $l->push(1); $l->unshift(0);
/// var_dump($l->pop(), $l->shift(), $l->isEmpty());
/// try { $l->pop(); } catch (RuntimeException $e) { echo $e->getMessage(), "\n"; }
/// try { $l->shift(); } catch (RuntimeException $e) { echo $e->getMessage(), "\n"; }
/// try { $l->top(); } catch (RuntimeException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// int(0)
/// bool(true)
/// Can't pop from an empty datastructure
/// Can't shift from an empty datastructure
/// Can't peek at an empty datastructure
/// ```
fn pop(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    match with(req, this, SplDllist::pop)? {
        Some(value) => Ok(value),
        None => Err(throw(
            req,
            "RuntimeException",
            "Can't pop from an empty datastructure",
        )),
    }
}

/// `shift(): mixed`; the witness is `pop`'s.
fn shift(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    match with(req, this, SplDllist::shift)? {
        Some(value) => Ok(value),
        None => Err(throw(
            req,
            "RuntimeException",
            "Can't shift from an empty datastructure",
        )),
    }
}

/// `push(mixed $value): void`.
fn push(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this(req)?;
    let value = crate::value(req, 1);
    let value = crate::copied(req, value);
    with(req, this, |l| l.push(value))?;
    Ok(Value::null())
}

/// `unshift(mixed $value): void`.
fn unshift(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this(req)?;
    let value = crate::value(req, 1);
    let value = crate::copied(req, value);
    with(req, this, |l| l.unshift(value))?;
    Ok(Value::null())
}

/// `top(): mixed`: the tail's data.
///
/// ```php
/// $l = new SplDoublyLinkedList; $l->push('first'); $l->push('last');
/// var_dump($l->top(), $l->bottom());
/// ```
/// ```expect
/// string(4) "last"
/// string(5) "first"
/// ```
fn top(req: &mut Request) -> Result<Value, Thrown> {
    peek(req, true)
}

/// `bottom(): mixed`: the head's data; the witness is `top`'s.
fn bottom(req: &mut Request) -> Result<Value, Thrown> {
    peek(req, false)
}

fn peek(req: &mut Request, tail: bool) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    let found = with(req, this, |l| {
        let at = if tail { l.tail } else { l.head };
        at.and_then(|at| l.nodes[at].data)
    })?;
    match found {
        Some(value) => Ok(crate::copied(req, value)),
        None => Err(throw(
            req,
            "RuntimeException",
            "Can't peek at an empty datastructure",
        )),
    }
}

/// `__debugInfo(): array`.
///
/// ```php
/// $q = new SplQueue; $q[] = 1;
/// var_dump($q);
/// ```
/// ```expect
/// object(SplQueue)#1 (2) {
///   ["flags":"SplDoublyLinkedList":private]=>
///   int(4)
///   ["dllist":"SplDoublyLinkedList":private]=>
///   array(1) {
///     [0]=>
///     int(1)
///   }
/// }
/// ```
fn debug_info(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    Ok(Value::array(list_debug_info(req, this)?))
}

/// `count(): int`.
fn count(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    let n = with(req, this, |l| l.count)?;
    Ok(Value::int(i64::try_from(n).unwrap_or(i64::MAX)))
}

/// `isEmpty(): bool`, over the class's count, `count()` a user class
/// overrides included.
fn is_empty(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    let n = Handlers.count_elements(req, this)?.unwrap_or(0);
    Ok(Value::bool(n == 0))
}

/// `setIteratorMode(int $mode): int`.
///
/// ```php
/// $l = new SplDoublyLinkedList; $l->push(1); $l->push(2); $l->push(3);
/// var_dump($l->setIteratorMode(SplDoublyLinkedList::IT_MODE_LIFO | SplDoublyLinkedList::IT_MODE_DELETE));
/// foreach ($l as $k => $v) echo "$k:$v ";
/// echo count($l), " ", $l->getIteratorMode(), "\n";
/// ```
/// ```expect
/// int(3)
/// 2:3 1:2 0:1 0 3
/// ```
fn set_iterator_mode(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this(req)?;
    let mode = crate::long(req, 1)?;
    let flags = with(req, this, |l| l.flags)?;
    if flags & IT_FIX != 0 && (flags & IT_LIFO) != (mode & IT_LIFO) {
        return Err(throw(
            req,
            "RuntimeException",
            "Iterators' LIFO/FIFO modes for SplStack/SplQueue objects are frozen",
        ));
    }
    let flags = (mode & IT_MASK) | (flags & IT_FIX);
    with(req, this, |l| l.flags = flags)?;
    Ok(Value::int(flags))
}

/// `getIteratorMode(): int`; the witness is `setIteratorMode`'s.
fn get_iterator_mode(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    Ok(Value::int(with(req, this, |l| l.flags)?))
}

/// `offsetExists($index): bool`.
///
/// ```php
/// $l = new SplDoublyLinkedList; $l->push('x'); $l->push('y');
/// var_dump(isset($l[1]), isset($l[2]), $l[1]);
/// $l[0] = 'z'; unset($l[1]);
/// var_dump($l->toArray ?? null, count($l), $l[0]);
/// try { $l[5]; } catch (OutOfRangeException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// string(1) "y"
/// NULL
/// int(1)
/// string(1) "z"
/// SplDoublyLinkedList::offsetGet(): Argument #1 ($index) is out of range
/// ```
fn offset_exists(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this(req)?;
    let index = crate::long(req, 1)?;
    let count = with(req, this, |l| l.count)?;
    Ok(Value::bool(
        index >= 0 && usize::try_from(index).is_ok_and(|i| i < count),
    ))
}

/// The node at `index`, or php's `OutOfRangeException`.
fn node(req: &mut Request, this: PhpObject, index: i64) -> Result<usize, Thrown> {
    let (count, found) = with(req, this, |l| {
        (l.count, l.offset(index, l.flags & IT_LIFO != 0))
    })?;
    if index < 0 || usize::try_from(index).is_ok_and(|i| i >= count) {
        return Err(out_of_range(req, "is out of range"));
    }
    found.ok_or_else(|| out_of_range(req, "is an invalid offset"))
}

/// `offsetGet($index): mixed`; the witness is `offsetExists`'s.
fn offset_get(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this(req)?;
    let index = crate::long(req, 1)?;
    let at = node(req, this, index)?;
    let value = with(req, this, |l| l.nodes[at].data)?.unwrap_or_default();
    Ok(crate::copied(req, value))
}

/// `offsetSet($index, mixed $value): void`, `null` pushing.
fn offset_set(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 2, 2)?;
    let this = this(req)?;
    let index = crate::nullable_long(req, 1)?;
    let value = crate::value(req, 2);
    let Some(index) = index else {
        let value = crate::copied(req, value);
        with(req, this, |l| l.push(value))?;
        return Ok(Value::null());
    };
    let at = node(req, this, index)?;
    let value = crate::copied(req, value);
    let old = with(req, this, |l| l.nodes[at].data.replace(value))?;
    if let Some(old) = old {
        req.release(old);
    }
    Ok(Value::null())
}

/// `offsetUnset($index): void`: the node unlinked, its links cut, the
/// traverse pointer let go where it stood on it.
fn offset_unset(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this(req)?;
    let index = crate::long(req, 1)?;
    let at = node(req, this, index)?;
    let old = with(req, this, |l| {
        let Node { prev, next, .. } = l.nodes[at];
        if let Some(prev) = prev {
            l.nodes[prev].next = next;
        }
        if let Some(next) = next {
            l.nodes[next].prev = prev;
        }
        if l.head == Some(at) {
            l.head = next;
        }
        if l.tail == Some(at) {
            l.tail = prev;
        }
        l.nodes[at].prev = None;
        l.nodes[at].next = None;
        l.count -= 1;
        if l.pointer == Some(at) {
            l.pointer = None;
        }
        l.nodes[at].data.take()
    })?;
    if let Some(old) = old {
        req.release(old);
    }
    Ok(Value::null())
}

/// `spl_dllist_it_helper_rewind`.
fn rewind(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    with(req, this, |l| {
        if l.flags & IT_LIFO != 0 {
            l.position = i64::try_from(l.count).unwrap_or(i64::MAX) - 1;
            l.pointer = l.tail;
        } else {
            l.position = 0;
            l.pointer = l.head;
        }
    })?;
    Ok(Value::null())
}

/// `current(): mixed`, `null` off the list.
fn current(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    let found = with(req, this, |l| l.pointer.and_then(|at| l.nodes[at].data))?;
    Ok(found.map_or_else(Value::null, |v| crate::copied(req, v)))
}

/// `key(): int`, the traverse position.
fn key(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    Ok(Value::int(with(req, this, |l| l.position)?))
}

/// `spl_dllist_it_helper_move_forward` under `flags`: the pointer one
/// step on, the element passed taken off under `IT_MODE_DELETE`.
fn move_forward(req: &mut Request, this: PhpObject, flags: i64) -> Result<(), Thrown> {
    let deleted = with(req, this, |l| {
        let old = l.pointer?;
        if flags & IT_LIFO != 0 {
            l.pointer = l.nodes[old].prev;
            l.position -= 1;
            if flags & IT_DELETE != 0 {
                return l.pop();
            }
        } else {
            l.pointer = l.nodes[old].next;
            if flags & IT_DELETE != 0 {
                return l.shift();
            }
            l.position += 1;
        }
        None
    })?;
    if let Some(value) = deleted {
        req.release(value);
    }
    Ok(())
}

/// `next(): void`.
///
/// ```php
/// $l = new SplDoublyLinkedList; foreach ([1, 2, 3] as $n) $l->push($n);
/// $l->rewind(); $l->next(); $l->next(); echo $l->current(), $l->key(), "\n";
/// $l->prev(); echo $l->current(), $l->key(), "\n";
/// $l->next(); $l->next(); var_dump($l->valid(), $l->current());
/// ```
/// ```expect
/// 32
/// 21
/// bool(false)
/// NULL
/// ```
fn next(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    let flags = with(req, this, |l| l.flags)?;
    move_forward(req, this, flags)?;
    Ok(Value::null())
}

/// `prev(): void`, `next()` the other way; the witness is `next`'s.
fn prev(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    let flags = with(req, this, |l| l.flags)?;
    move_forward(req, this, flags ^ IT_LIFO)?;
    Ok(Value::null())
}

/// `valid(): bool`.
fn valid(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    Ok(Value::bool(with(req, this, |l| l.pointer.is_some())?))
}

/// `serialize(): string`: the flags, then `:` and each element, each
/// through `serialize()`.
///
/// The witness waits on the ext/standard native it calls (#712).
fn serialize(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    let (flags, values) = with(req, this, |l| (l.flags, l.values()))?;
    let mut out = crate::serialized(req, Value::int(flags))?;
    for value in values {
        out.push(b':');
        out.extend(crate::serialized(req, value)?);
    }
    crate::string_value(req, &out)
}

/// `unserialize(string $data): void`; the witness is `serialize`'s.
fn unserialize(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this(req)?;
    let data = crate::string(req, 1)?.as_bytes().to_vec();
    if data.is_empty() {
        return Ok(Value::null());
    }
    while let Some(old) = with(req, this, SplDllist::pop)? {
        req.release(old);
    }
    let refuse = |req: &mut Request, at: usize| {
        Err(throw(
            req,
            "UnexpectedValueException",
            &format!("Error at offset {at} of {} bytes", data.len()),
        ))
    };
    let Some(end) = crate::serialized_end(&data, 0) else {
        return refuse(req, 0);
    };
    let Some(flags) = crate::unserialized(req, &data[..end])?.and_then(|f| f.as_int()) else {
        return refuse(req, 0);
    };
    with(req, this, |l| l.flags = flags)?;
    let mut at = end;
    while data.get(at) == Some(&b':') {
        let Some(end) = crate::serialized_end(&data, at + 1) else {
            return refuse(req, at + 1);
        };
        let Some(value) = crate::unserialized(req, &data[at + 1..end])? else {
            return refuse(req, at + 1);
        };
        with(req, this, |l| l.push(value))?;
        at = end;
    }
    if at != data.len() {
        return refuse(req, at);
    }
    Ok(Value::null())
}

/// `__serialize(): array`: the flags, the elements and the members.
///
/// ```php
/// $s = new SplStack; $s->push('a');
/// var_dump($s->__serialize());
/// $t = new SplStack; $t->__unserialize([6, ['x', 'y'], []]); echo $t->top(), count($t), "\n";
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   int(6)
///   [1]=>
///   array(1) {
///     [0]=>
///     string(1) "a"
///   }
///   [2]=>
///   array(0) {
///   }
/// }
/// y2
/// ```
fn magic_serialize(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this(req)?;
    let (flags, values) = with(req, this, |l| (l.flags, l.values()))?;
    let mut out = PhpArray::new(req)?;
    out.append(req, Value::int(flags))?;
    let mut list = PhpArray::new(req)?;
    for value in values {
        list.append(req, crate::copied(req, value))?;
    }
    out.append(req, Value::array(list))?;
    let members = crate::members(req, this)?;
    out.append(req, Value::array(members))?;
    Ok(Value::array(out))
}

/// `__unserialize(array $data): void`; the witness is `__serialize`'s.
fn magic_unserialize(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this(req)?;
    let data = crate::array(req, 1)?;
    let flags = data.get(&Key::Int(0)).and_then(|f| f.deref().as_int());
    let storage = data
        .get(&Key::Int(1))
        .and_then(|s| s.deref().as_array().copied());
    let members = data
        .get(&Key::Int(2))
        .and_then(|m| m.deref().as_array().copied());
    let (Some(flags), Some(storage), Some(members)) = (flags, storage, members) else {
        return Err(throw(
            req,
            "UnexpectedValueException",
            "Incomplete or ill-typed serialization data",
        ));
    };
    with(req, this, |l| l.flags = flags)?;
    for (_, value) in storage.snapshot() {
        let value = crate::copied(req, value);
        with(req, this, |l| l.push(value))?;
    }
    for (key, value) in members.snapshot() {
        let name = key.to_prop_name(req)?;
        req.addref(&value);
        Std.write_property(req, this, name, value)?;
    }
    Ok(Value::null())
}
