//! `spl_dllist.c`: `SplDoublyLinkedList`, `SplQueue` and `SplStack`
//! over php's `spl_ptr_llist`. The nodes stand in an arena, each with
//! its neighbours by index, so a node the traverse pointer stands on
//! keeps its links as php's refcounted element does when it is popped,
//! shifted or unset under the pointer: a node taken out has its links
//! cut on the side php cuts them, and the walk ends there as php's
//! does. `IT_MODE_LIFO` walks from the tail, `IT_MODE_DELETE` takes
//! each element off as the walk passes it, and `SplQueue` and
//! `SplStack` freeze the direction (`SPL_DLLIST_IT_FIX`).

use php_ext::abi::{
    ClassIterator, CloneObj, ExtensionBuilder, ObjectHandlers, PropertyPurpose, This, Untyped,
};
use php_ext::attr::php_class;
use php_ext::call::{call_method, clone_with};
use php_ext::coerce::to_long;
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, Value};

use crate::{ArrayAccess, Countable, Iterator, Serializable};

/// `SPL_DLLIST_IT_MASK`, the two bits `setIteratorMode()` sets.
const IT_MASK: i64 = 3;
/// `SPL_DLLIST_IT_FIX`: the LIFO bit frozen, `SplQueue`'s and `SplStack`'s.
const IT_FIX: i64 = 4;

/// One `spl_ptr_llist_element`: its data, `None` once taken out, and
/// its neighbours.
#[derive(Clone, Copy, Default)]
struct Node {
    data: Option<Value>,
    prev: Option<usize>,
    next: Option<usize>,
}

/// A traverse pointer and its position, the object's own or a
/// `foreach`'s: `traverse_pointer` and `traverse_position`, which
/// `spl_dllist_object` and `spl_dllist_it` each keep and the
/// `spl_dllist_it_helper_*` functions move.
#[derive(Clone, Copy, Default)]
struct Cursor {
    pointer: Option<usize>,
    position: i64,
}

/// `spl_dllist_object`: the list, the traverse cursor and the flags.
#[derive(Default)]
pub(crate) struct Llist {
    nodes: Vec<Node>,
    head: Option<usize>,
    tail: Option<usize>,
    count: usize,
    cursor: Cursor,
    flags: i64,
    ready: bool,
}

impl Llist {
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

    /// `spl_dllist_it_helper_rewind`: the cursor at the tail under
    /// LIFO, at the head otherwise.
    fn rewind(&self, cursor: &mut Cursor, flags: i64) {
        *cursor = if flags & SplDoublyLinkedList::IT_MODE_LIFO != 0 {
            Cursor {
                pointer: self.tail,
                position: i64::try_from(self.count).unwrap_or(i64::MAX) - 1,
            }
        } else {
            Cursor {
                pointer: self.head,
                position: 0,
            }
        };
    }

    /// `spl_dllist_it_helper_move_forward`: the cursor one step on under
    /// `flags`, and the element passed, taken off under
    /// `IT_MODE_DELETE`, for the caller to give up.
    fn move_forward(&mut self, cursor: &mut Cursor, flags: i64) -> Option<Value> {
        let old = cursor.pointer?;
        if flags & SplDoublyLinkedList::IT_MODE_LIFO != 0 {
            cursor.pointer = self.nodes[old].prev;
            cursor.position -= 1;
            if flags & SplDoublyLinkedList::IT_MODE_DELETE != 0 {
                return self.pop();
            }
        } else {
            cursor.pointer = self.nodes[old].next;
            if flags & SplDoublyLinkedList::IT_MODE_DELETE != 0 {
                return self.shift();
            }
            cursor.position += 1;
        }
        None
    }

    /// The data a cursor stands on, `None` off the list or on a node
    /// taken out under it.
    fn at(&self, cursor: Cursor) -> Option<Value> {
        cursor.pointer.and_then(|at| self.nodes[at].data)
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

/// The three classes onto `builder`, in the order
/// `PHP_MINIT_FUNCTION(spl_dllist)` registers them.
pub(crate) fn register(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder
        .class::<SplDoublyLinkedList>()
        .class::<SplQueue>()
        .class::<SplStack>()
}

/// `class SplDoublyLinkedList implements Iterator, Countable,
/// ArrayAccess, Serializable`.
///
/// ```php
/// $l = new SplDoublyLinkedList;
/// $l->push(2); $l->unshift(1); $l[] = 3;
/// echo count($l), $l->count(), " ", $l->offsetExists(2) ? "y" : "n", $l->offsetGet(0), "\n";
/// $l->offsetSet(0, 'one'); $l->offsetSet(null, 4); $l->offsetUnset(3);
/// foreach ($l as $k => $v) echo "$k:$v;";
/// echo "\n";
/// $l->rewind(); var_dump($l->valid(), $l->key(), $l->current());
/// $l->next(); $l->prev(); var_dump($l->current(), $l->bottom(), $l->shift(), $l->isEmpty(), $l->getIteratorMode());
/// echo $l->serialize(), "\n";
/// $m = new SplDoublyLinkedList; $m->__unserialize($l->__serialize()); var_dump(count($m), $m->__debugInfo()["\0SplDoublyLinkedList\0flags"]);
/// ```
/// ```expect
/// 33 y1
/// 0:one;1:2;2:3;
/// bool(true)
/// int(0)
/// string(3) "one"
/// string(3) "one"
/// string(3) "one"
/// string(3) "one"
/// bool(false)
/// int(0)
/// i:0;:i:2;:i:3;
/// int(2)
/// int(0)
/// ```
#[php_class(
    implements(Iterator, Countable, ArrayAccess, Serializable),
    payload = Llist,
    handlers = HANDLERS
)]
pub(crate) struct SplDoublyLinkedList;

#[php_class]
impl SplDoublyLinkedList {
    const IT_MODE_LIFO: i64 = 2;
    const IT_MODE_FIFO: i64 = 0;
    const IT_MODE_DELETE: i64 = 1;
    const IT_MODE_KEEP: i64 = 0;

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
    #[php(tentative)]
    fn add(req: &mut Request, this: This<Self>, index: i64, value: Value) -> Result<(), Thrown> {
        let this = this.object();
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
            let Some(element) = l.offset(index, l.flags & Self::IT_MODE_LIFO != 0) else {
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
        })
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
    #[php(tentative)]
    fn pop(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        match with(req, this.object(), Llist::pop)? {
            Some(value) => Ok(value),
            None => Err(throw(
                req,
                "RuntimeException",
                "Can't pop from an empty datastructure",
            )),
        }
    }

    /// `shift(): mixed`; the witness is the class's.
    #[php(tentative)]
    fn shift(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        match with(req, this.object(), Llist::shift)? {
            Some(value) => Ok(value),
            None => Err(throw(
                req,
                "RuntimeException",
                "Can't shift from an empty datastructure",
            )),
        }
    }

    /// `push(mixed $value): void`.
    #[php(tentative)]
    fn push(req: &mut Request, this: This<Self>, value: Value) -> Result<(), Thrown> {
        let value = crate::copied(req, value);
        with(req, this.object(), |l| l.push(value))
    }

    /// `unshift(mixed $value): void`.
    #[php(tentative)]
    fn unshift(req: &mut Request, this: This<Self>, value: Value) -> Result<(), Thrown> {
        let value = crate::copied(req, value);
        with(req, this.object(), |l| l.unshift(value))
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
    #[php(tentative)]
    fn top(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        peek(req, this.object(), true)
    }

    /// `bottom(): mixed`: the head's data; the witness is the class's.
    #[php(tentative)]
    fn bottom(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        peek(req, this.object(), false)
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
    #[php(tentative)]
    fn __debugInfo(req: &mut Request, this: This<Self>) -> Result<PhpArray, Thrown> {
        list_debug_info(req, this.object())
    }

    /// `count(): int`.
    #[php(tentative)]
    fn count(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let n = with(req, this.object(), |l| l.count)?;
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// `isEmpty(): bool`, over the class's count, `count()` a user class
    /// overrides included.
    #[php(tentative)]
    fn isEmpty(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = this.object();
        ready(req, this)?;
        let n = Handlers.count_elements(req, this)?.unwrap_or(0);
        Ok(n == 0)
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
    #[php(tentative)]
    fn setIteratorMode(req: &mut Request, this: This<Self>, mode: i64) -> Result<i64, Thrown> {
        let this = this.object();
        let flags = with(req, this, |l| l.flags)?;
        if flags & IT_FIX != 0 && (flags & Self::IT_MODE_LIFO) != (mode & Self::IT_MODE_LIFO) {
            return Err(throw(
                req,
                "RuntimeException",
                "Iterators' LIFO/FIFO modes for SplStack/SplQueue objects are frozen",
            ));
        }
        let flags = (mode & IT_MASK) | (flags & IT_FIX);
        with(req, this, |l| l.flags = flags)?;
        Ok(flags)
    }

    /// `getIteratorMode(): int`; the witness is `setIteratorMode`'s.
    #[php(tentative)]
    fn getIteratorMode(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        with(req, this.object(), |l| l.flags)
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
    #[php(tentative)]
    fn offsetExists(
        req: &mut Request,
        this: This<Self>,
        index: Untyped<i64>,
    ) -> Result<bool, Thrown> {
        let index = index.0;
        let count = with(req, this.object(), |l| l.count)?;
        Ok(index >= 0 && usize::try_from(index).is_ok_and(|i| i < count))
    }

    /// `offsetGet($index): mixed`; the witness is `offsetExists`'s.
    #[php(tentative)]
    fn offsetGet(
        req: &mut Request,
        this: This<Self>,
        index: Untyped<i64>,
    ) -> Result<Value, Thrown> {
        let this = this.object();
        let at = node(req, this, index.0)?;
        let value = with(req, this, |l| l.nodes[at].data)?.unwrap_or_default();
        Ok(crate::copied(req, value))
    }

    /// `offsetSet($index, mixed $value): void`, `null` pushing.
    #[php(tentative)]
    fn offsetSet(
        req: &mut Request,
        this: This<Self>,
        index: Untyped<Option<i64>>,
        value: Value,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let Some(index) = index.0 else {
            let value = crate::copied(req, value);
            return with(req, this, |l| l.push(value));
        };
        let at = node(req, this, index)?;
        let value = crate::copied(req, value);
        let old = with(req, this, |l| l.nodes[at].data.replace(value))?;
        if let Some(old) = old {
            req.release(old);
        }
        Ok(())
    }

    /// `offsetUnset($index): void`: the node unlinked, its links cut, the
    /// traverse pointer let go where it stood on it.
    #[php(tentative)]
    fn offsetUnset(req: &mut Request, this: This<Self>, index: Untyped<i64>) -> Result<(), Thrown> {
        let this = this.object();
        let at = node(req, this, index.0)?;
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
            if l.cursor.pointer == Some(at) {
                l.cursor.pointer = None;
            }
            l.nodes[at].data.take()
        })?;
        if let Some(old) = old {
            req.release(old);
        }
        Ok(())
    }

    /// `spl_dllist_it_helper_rewind`.
    #[php(tentative)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        with(req, this.object(), |l| {
            let mut cursor = l.cursor;
            l.rewind(&mut cursor, l.flags);
            l.cursor = cursor;
        })
    }

    /// `current(): mixed`, `null` off the list.
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let found = with(req, this.object(), |l| l.at(l.cursor))?;
        Ok(found.map_or_else(Value::null, |v| crate::copied(req, v)))
    }

    /// `key(): int`, the traverse position.
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        with(req, this.object(), |l| l.cursor.position)
    }

    /// `prev(): void`, `next()` the other way; the witness is `next`'s.
    #[php(tentative)]
    fn prev(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = this.object();
        let flags = with(req, this, |l| l.flags)?;
        move_forward(req, this, flags ^ Self::IT_MODE_LIFO)
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
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = this.object();
        let flags = with(req, this, |l| l.flags)?;
        move_forward(req, this, flags)
    }

    /// `valid(): bool`.
    #[php(tentative)]
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        with(req, this.object(), |l| l.cursor.pointer.is_some())
    }

    /// `unserialize(string $data): void`: the list emptied, then the flags
    /// and each `:`-led element read through one var hash, as php reads
    /// them; where the data breaks off php's `UnexpectedValueException`
    /// names the offset the reader stopped at.
    ///
    /// ```php
    /// $l = new SplDoublyLinkedList;
    /// $l->unserialize('i:2;:s:1:"a";:O:8:"stdClass":0:{}:r:3;');
    /// var_dump($l->getIteratorMode(), count($l), $l[1] === $l[2]);
    /// foreach (['i:0;:x', 's:1:"a";', 'i:1;;'] as $bad) {
    ///     try { $l->unserialize($bad); } catch (UnexpectedValueException $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// int(2)
    /// int(3)
    /// bool(false)
    /// Error at offset 5 of 6 bytes
    /// Error at offset 8 of 8 bytes
    /// Error at offset 4 of 5 bytes
    /// ```
    #[php(tentative)]
    fn unserialize(req: &mut Request, this: This<Self>, data: PhpString) -> Result<(), Thrown> {
        let this = this.object();
        let data = data.as_bytes().to_vec();
        if data.is_empty() {
            return Ok(());
        }
        while let Some(old) = with(req, this, Llist::pop)? {
            req.release(old);
        }
        let mut reader = php_ext_standard::Unserializer::new(req, &data);
        let read = read_list(req, this, &mut reader, &data);
        let finished = reader.finish(req);
        let broke_at = read?;
        finished?;
        match broke_at {
            Some(at) => Err(throw(
                req,
                "UnexpectedValueException",
                &format!("Error at offset {at} of {} bytes", data.len()),
            )),
            None => Ok(()),
        }
    }

    /// `serialize(): string`: the flags, then `:` and each element, each
    /// through `serialize()`, walking the nodes as they stand: the next
    /// node is read before an element is written, and the walk ends at
    /// one an element's `__serialize()` took out of the list
    /// (`SPL_LLIST_CHECK_DELREF_EX(next, break;)`).
    ///
    /// ```php
    /// class C { function __serialize(): array { global $list; $list->pop(); return []; } }
    /// $list = new SplDoublyLinkedList;
    /// $list->push(new C); $list->push(1); $list->push(2);
    /// var_dump($list->serialize(), count($list));
    /// ```
    /// ```expect
    /// string(22) "i:0;:O:1:"C":0:{}:i:1;"
    /// int(2)
    /// ```
    #[php(tentative)]
    fn serialize(req: &mut Request, this: This<Self>) -> Result<PhpString, Thrown> {
        let this = this.object();
        let (flags, head) = with(req, this, |l| (l.flags, l.head))?;
        crate::serialized_parts(req, |req, serializer, out| {
            serializer.write(req, out, &Value::int(flags))?;
            let mut current = head;
            while let Some(at) = current {
                out.push(b':');
                let (data, next) = with(req, this, |l| (l.nodes[at].data, l.nodes[at].next))?;
                let data = data.unwrap_or_default();
                req.addref(&data);
                let written = serializer.write(req, out, &data);
                req.release(data);
                written?;
                let taken = match next {
                    Some(next) => with(req, this, |l| l.nodes[next].data.is_none())?,
                    None => false,
                };
                if taken {
                    break;
                }
                current = next;
            }
            Ok(())
        })
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
    #[php(tentative)]
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<PhpArray, Thrown> {
        let this = this.object();
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
        Ok(out)
    }

    /// `__unserialize(array $data): void`; the witness is the class's.
    #[php(tentative)]
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        let this = this.object();
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
        crate::load_members(req, this, &Value::array(members))
    }
}

/// `class SplQueue extends SplDoublyLinkedList`, its walk FIFO and
/// frozen so; `enqueue` and `dequeue` are php's aliases of `push` and
/// `shift`.
///
/// ```php
/// $q = new SplQueue; $q->enqueue('a'); $q->enqueue('b');
/// echo $q->dequeue(), count($q), "\n";
/// try { $q->setIteratorMode(SplDoublyLinkedList::IT_MODE_LIFO); } catch (RuntimeException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// a1
/// Iterators' LIFO/FIFO modes for SplStack/SplQueue objects are frozen
/// ```
#[php_class(extends(SplDoublyLinkedList))]
pub(crate) struct SplQueue;

#[php_class]
impl SplQueue {
    #[php(tentative, implementation = SplDoublyLinkedList::push)]
    fn enqueue(this: This<Self>, value: Value);
    #[php(tentative, implementation = SplDoublyLinkedList::shift)]
    fn dequeue(this: This<Self>) -> Value;
}

/// `class SplStack extends SplDoublyLinkedList`, its walk LIFO and
/// frozen so.
///
/// ```php
/// $s = new SplStack; $s->push(1); $s->push(2); $s[] = 3;
/// foreach ($s as $k => $v) echo "$k:$v;";
/// echo "\n", $s->top(), " ", $s->pop(), " ", count($s), "\n";
/// ```
/// ```expect
/// 2:3;1:2;0:1;
/// 3 3 2
/// ```
#[php_class(extends(SplDoublyLinkedList))]
pub(crate) struct SplStack;

#[php_class]
impl SplStack {}

/// The payload of `object`, the flags laid down on first use as
/// `spl_dllist_object_new_ex` lays them for the class.
fn ready(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    match object.native::<Llist>(req).map(|l| l.ready) {
        Some(true) => Ok(()),
        Some(false) => {
            let flags = if crate::is_a(req, object.class(), b"SplStack") {
                IT_FIX | SplDoublyLinkedList::IT_MODE_LIFO
            } else if crate::is_a(req, object.class(), b"SplQueue") {
                IT_FIX
            } else {
                0
            };
            if let Some(l) = object.native_mut::<Llist>(req) {
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
    change: impl FnOnce(&mut Llist) -> R,
) -> Result<R, Thrown> {
    ready(req, object)?;
    match object.native_mut::<Llist>(req) {
        Some(l) => Ok(change(l)),
        None => Ok(change(&mut Llist::default())),
    }
}

/// `zend_argument_error(spl_ce_OutOfRangeException, 1, ...)`.
fn out_of_range(req: &mut Request, tail: &str) -> Thrown {
    php_ext::raising::argument_error(req, "OutOfRangeException", 1, tail)
}

impl ObjectHandlers for Handlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let values = object
            .native_mut::<Llist>(req)
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
        if let Some(l) = object.native::<Llist>(req) {
            for value in l.values() {
                visit(value);
            }
        }
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_list)
    }

    /// `spl_dllist_get_iterator`: a cursor of the loop's own, where
    /// the object's stands and under the direction the flags set when
    /// the loop began, so a loop inside a loop over one list, or a
    /// change of direction in the body, moves no other walk.
    ///
    /// ```php
    /// $l = new SplDoublyLinkedList; $l->push(1); $l->push(2); $l->push(3);
    /// foreach ($l as $k => $v) { $l->setIteratorMode(SplDoublyLinkedList::IT_MODE_LIFO); foreach ($l as $w) echo $w; echo " $k=$v\n"; }
    /// $l->setIteratorMode(SplDoublyLinkedList::IT_MODE_DELETE);
    /// foreach ($l as $k => $v) echo "$k=$v ";
    /// echo count($l), "\n";
    /// try { foreach (new SplQueue as &$v) {} } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// 321 0=1
    /// 321 1=2
    /// 321 2=3
    /// 0=1 0=2 0=3 0
    /// An iterator cannot be used with foreach by reference
    /// ```
    fn get_iterator(
        &self,
        req: &mut Request,
        object: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        if by_ref {
            return Err(throw(
                req,
                "Error",
                "An iterator cannot be used with foreach by reference",
            ));
        }
        let (cursor, flags) = with(req, object, |l| (l.cursor, l.flags & IT_MASK))?;
        Ok(Some(Box::new(ListIterator { cursor, flags })))
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

/// `spl_dllist_it`: the loop's cursor and the direction it walks in.
struct ListIterator {
    cursor: Cursor,
    flags: i64,
}

impl ClassIterator for ListIterator {
    fn rewind(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        let flags = self.flags;
        with(req, object, |l| l.rewind(&mut self.cursor, flags))
    }

    fn valid(&mut self, _: &mut Request, _: PhpObject) -> Result<bool, Thrown> {
        Ok(self.cursor.pointer.is_some())
    }

    fn current(&mut self, req: &mut Request, object: PhpObject) -> Result<Value, Thrown> {
        let found = with(req, object, |l| l.at(self.cursor))?;
        Ok(found.map_or_else(Value::null, |v| crate::copied(req, v)))
    }

    fn key(&mut self, _: &mut Request, _: PhpObject) -> Result<Value, Thrown> {
        Ok(Value::int(self.cursor.position))
    }

    fn move_forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        let flags = self.flags;
        if let Some(value) = with(req, object, |l| l.move_forward(&mut self.cursor, flags))? {
            req.release(value);
        }
        Ok(())
    }
}

/// `spl_dllist_object_clone`: the list copied, one more owner of each
/// element, the pointer at the head.
fn clone_list(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let (values, flags) = with(req, object, |l| (l.values(), l.flags))?;
    let mut copy = Llist {
        flags,
        ready: true,
        ..Llist::default()
    };
    for value in values {
        req.addref(&value);
        copy.push(value);
    }
    copy.cursor.pointer = copy.head;
    clone_with(req, &object, copy)
}

/// `spl_dllist_object_get_debug_info`: the properties, then `flags` and
/// `dllist` under `SplDoublyLinkedList`'s private names.
fn list_debug_info(req: &mut Request, object: PhpObject) -> Result<PhpArray, Thrown> {
    let (flags, values) = with(req, object, |l| (l.flags, l.values()))?;
    let mut out = PhpArray::new(req)?;
    for (key, value) in object.props_for(req, PropertyPurpose::Debug)? {
        req.addref(&value);
        out.set(req, key, value)?;
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

/// The tail's or the head's data, php's `Can't peek` where there is none.
fn peek(req: &mut Request, this: PhpObject, tail: bool) -> Result<Value, Thrown> {
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

/// The node at `index`, or php's `OutOfRangeException`.
fn node(req: &mut Request, this: PhpObject, index: i64) -> Result<usize, Thrown> {
    let (count, found) = with(req, this, |l| {
        (
            l.count,
            l.offset(index, l.flags & SplDoublyLinkedList::IT_MODE_LIFO != 0),
        )
    })?;
    if index < 0 || usize::try_from(index).is_ok_and(|i| i >= count) {
        return Err(out_of_range(req, "is out of range"));
    }
    found.ok_or_else(|| out_of_range(req, "is an invalid offset"))
}

/// `spl_dllist_it_helper_move_forward` under `flags`: the pointer one
/// step on, the element passed taken off under `IT_MODE_DELETE`.
fn move_forward(req: &mut Request, this: PhpObject, flags: i64) -> Result<(), Thrown> {
    let deleted = with(req, this, |l| {
        let mut cursor = l.cursor;
        let deleted = l.move_forward(&mut cursor, flags);
        l.cursor = cursor;
        deleted
    })?;
    if let Some(value) = deleted {
        req.release(value);
    }
    Ok(())
}

/// The body of `SplDoublyLinkedList::unserialize` over one reader:
/// `None` once the data is read, or the offset php's `error` label
/// reports. Each value read is the var hash's, php's `var_tmp_var`, and
/// the list takes an owner of each element.
fn read_list(
    req: &mut Request,
    this: PhpObject,
    reader: &mut php_ext_standard::Unserializer<'_>,
    data: &[u8],
) -> Result<Option<usize>, Thrown> {
    let Some(flags) = reader.value(req)? else {
        return Ok(Some(reader.offset()));
    };
    let Some(flags) = flags.as_int() else {
        return Ok(Some(reader.offset()));
    };
    with(req, this, |l| l.flags = flags)?;
    while data.get(reader.offset()) == Some(&b':') {
        reader.set_offset(reader.offset() + 1);
        let Some(value) = reader.value(req)? else {
            return Ok(Some(reader.offset()));
        };
        req.addref(&value);
        with(req, this, |l| l.push(value))?;
    }
    if reader.offset() != data.len() {
        return Ok(Some(reader.offset()));
    }
    Ok(None)
}
