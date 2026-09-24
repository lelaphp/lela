//! `spl_iterators.stub.php`: the iterator interfaces and classes, as
//! `PHP_MINIT_FUNCTION(spl_iterators)` registers them, in its order.
//! The outer iterators are php's `spl_dual_it_object` ([`DualIt`]):
//! the inner iterator, the element fetched off it and its key, the
//! position, and what each kind keeps beside them; the recursive ones
//! are `spl_recursive_it_object` ([`RecIt`]), a stack of sub-iterators
//! each with its state, walked by `spl_recursive_it_move_forward_ex`'s
//! state machine. An inner iterator is served through its methods,
//! which is what php's `zend_user_it_*` functions do for a user class
//! and what a native class's methods do for its own, so the order of
//! the calls an inner iterator sees is php's. `RegexIterator` and
//! `RecursiveRegexIterator` wait on ext/pcre's compiled-pattern face
//! (#713).

use php_ext::abi::{Callable, CloneObj, ObjectHandlers};
use php_ext::call::{call, call_method, instantiate, lookup_class};
use php_ext::coerce::{callable_checked, to_string, truthy};
use php_ext::raising::{Thrown, arg_type_error, arg_value_error, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, Value, ValueRef};

use crate::Entry;
use crate::stub::{self, Declared};

const RIT_LEAVES_ONLY: i64 = 0;
const RIT_SELF_FIRST: i64 = 1;
const RIT_CHILD_FIRST: i64 = 2;
const CIT_CALL_TOSTRING: i64 = 1;
const CIT_TOSTRING_USE_KEY: i64 = 2;
const CIT_TOSTRING_USE_CURRENT: i64 = 4;
const CIT_TOSTRING_USE_INNER: i64 = 8;
const CIT_CATCH_GET_CHILD: i64 = 16;
const CIT_FULL_CACHE: i64 = 256;
const CIT_PUBLIC: i64 = 0x0000_FFFF;
const CIT_VALID: i64 = 0x0001_0000;
const RTIT_BYPASS_CURRENT: i64 = 4;
const RTIT_BYPASS_KEY: i64 = 8;

/// `dual_it_type`: which outer iterator the object is, `Unknown` until
/// its constructor ran.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Kind {
    #[default]
    Unknown,
    Default,
    Limit,
    Caching,
    RecursiveCaching,
    IteratorIterator,
    NoRewind,
    Infinite,
    Append,
    CallbackFilter,
    RecursiveCallbackFilter,
}

/// `spl_dual_it_object`.
#[derive(Default)]
pub(crate) struct DualIt {
    kind: Kind,
    inner: Option<PhpObject>,
    data: Option<Value>,
    key: Option<Value>,
    pos: i64,
    offset: i64,
    count: i64,
    flags: i64,
    cache: Option<PhpArray>,
    children: Option<PhpObject>,
    string: Option<Value>,
    callback: Option<Callable>,
    callback_value: Option<Value>,
    appended: Option<PhpObject>,
}

/// One level of `spl_recursive_it_object`'s stack.
#[derive(Clone, Copy)]
struct Level {
    iterator: PhpObject,
    state: State,
}

/// `RecursiveIteratorState`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Next,
    Test,
    SelfFirst,
    Child,
    Start,
}

/// `spl_recursive_it_object`, and the prefix parts and postfix
/// `RecursiveTreeIterator` draws with.
#[derive(Default)]
pub(crate) struct RecIt {
    levels: Vec<Level>,
    mode: i64,
    flags: i64,
    max_depth: i64,
    in_iteration: bool,
    prefix: Option<[Vec<u8>; 6]>,
    postfix: Vec<u8>,
}

/// `spl_handlers_dual_it`.
pub(crate) struct DualHandlers;

pub(crate) static DUAL: DualHandlers = DualHandlers;

/// `spl_handlers_rec_it_it`.
pub(crate) struct RecHandlers;

pub(crate) static REC: RecHandlers = RecHandlers;

const STUB: &str = "
interface RecursiveIterator extends Iterator {
    /** @tentative-return-type */ public function hasChildren(): bool;
    /** @tentative-return-type */ public function getChildren(): ?RecursiveIterator;
}
interface OuterIterator extends Iterator {
    /** @tentative-return-type */ public function getInnerIterator(): ?Iterator;
}
class RecursiveIteratorIterator implements OuterIterator {
    public const int LEAVES_ONLY = 0;
    public const int SELF_FIRST = 1;
    public const int CHILD_FIRST = 2;
    public const int CATCH_GET_CHILD = 16;
    public function __construct(Traversable $iterator, int $mode = RecursiveIteratorIterator::LEAVES_ONLY, int $flags = 0) {}
    /** @tentative-return-type */ public function rewind(): void {}
    /** @tentative-return-type */ public function valid(): bool {}
    /** @tentative-return-type */ public function key(): mixed {}
    /** @tentative-return-type */ public function current(): mixed {}
    /** @tentative-return-type */ public function next(): void {}
    /** @tentative-return-type */ public function getDepth(): int {}
    /** @tentative-return-type */ public function getSubIterator(?int $level = null): ?RecursiveIterator {}
    /** @tentative-return-type */ public function getInnerIterator(): RecursiveIterator {}
    /** @tentative-return-type */ public function beginIteration(): void {}
    /** @tentative-return-type */ public function endIteration(): void {}
    /** @tentative-return-type */ public function callHasChildren(): bool {}
    /** @tentative-return-type */ public function callGetChildren(): ?RecursiveIterator {}
    /** @tentative-return-type */ public function beginChildren(): void {}
    /** @tentative-return-type */ public function endChildren(): void {}
    /** @tentative-return-type */ public function nextElement(): void {}
    /** @tentative-return-type */ public function setMaxDepth(int $maxDepth = -1): void {}
    /** @tentative-return-type */ public function getMaxDepth(): int|false {}
}
class IteratorIterator implements OuterIterator {
    public function __construct(Traversable $iterator, ?string $class = null) {}
    /** @tentative-return-type */ public function getInnerIterator(): ?Iterator {}
    /** @tentative-return-type */ public function rewind(): void {}
    /** @tentative-return-type */ public function valid(): bool {}
    /** @tentative-return-type */ public function key(): mixed {}
    /** @tentative-return-type */ public function current(): mixed {}
    /** @tentative-return-type */ public function next(): void {}
}
abstract class FilterIterator extends IteratorIterator {
    /** @tentative-return-type */ abstract public function accept(): bool;
    public function __construct(Iterator $iterator) {}
    /** @tentative-return-type */ public function rewind(): void {}
    /** @tentative-return-type */ public function next(): void {}
}
abstract class RecursiveFilterIterator extends FilterIterator implements RecursiveIterator {
    public function __construct(RecursiveIterator $iterator) {}
    /** @tentative-return-type */ public function hasChildren(): bool {}
    /** @tentative-return-type */ public function getChildren(): ?RecursiveFilterIterator {}
}
class CallbackFilterIterator extends FilterIterator {
    public function __construct(Iterator $iterator, callable $callback) {}
    /** @tentative-return-type */ public function accept(): bool {}
}
class RecursiveCallbackFilterIterator extends CallbackFilterIterator implements RecursiveIterator {
    public function __construct(RecursiveIterator $iterator, callable $callback) {}
    /** @tentative-return-type */ public function hasChildren(): bool {}
    /** @tentative-return-type */ public function getChildren(): RecursiveCallbackFilterIterator {}
}
class ParentIterator extends RecursiveFilterIterator {
    public function __construct(RecursiveIterator $iterator) {}
    /** @tentative-return-type */ public function accept(): bool {}
}
interface SeekableIterator extends Iterator {
    /** @tentative-return-type */ public function seek(int $offset): void;
}
class LimitIterator extends IteratorIterator {
    public function __construct(Iterator $iterator, int $offset = 0, int $limit = -1) {}
    /** @tentative-return-type */ public function rewind(): void {}
    /** @tentative-return-type */ public function valid(): bool {}
    /** @tentative-return-type */ public function next(): void {}
    /** @tentative-return-type */ public function seek(int $offset): int {}
    /** @tentative-return-type */ public function getPosition(): int {}
}
class CachingIterator extends IteratorIterator implements ArrayAccess, Countable, Stringable {
    public const int CALL_TOSTRING = 1;
    public const int CATCH_GET_CHILD = 16;
    public const int TOSTRING_USE_KEY = 2;
    public const int TOSTRING_USE_CURRENT = 4;
    public const int TOSTRING_USE_INNER = 8;
    public const int FULL_CACHE = 256;
    public function __construct(Iterator $iterator, int $flags = CachingIterator::CALL_TOSTRING) {}
    /** @tentative-return-type */ public function rewind(): void {}
    /** @tentative-return-type */ public function valid(): bool {}
    /** @tentative-return-type */ public function next(): void {}
    /** @tentative-return-type */ public function hasNext(): bool {}
    public function __toString(): string {}
    /** @tentative-return-type */ public function getFlags(): int {}
    /** @tentative-return-type */ public function setFlags(int $flags): void {}
    /** @tentative-return-type */ public function offsetGet($key): mixed {}
    /** @tentative-return-type */ public function offsetSet($key, mixed $value): void {}
    /** @tentative-return-type */ public function offsetUnset($key): void {}
    /** @tentative-return-type */ public function offsetExists($key): bool {}
    /** @tentative-return-type */ public function getCache(): array {}
    /** @tentative-return-type */ public function count(): int {}
}
class RecursiveCachingIterator extends CachingIterator implements RecursiveIterator {
    public function __construct(Iterator $iterator, int $flags = RecursiveCachingIterator::CALL_TOSTRING) {}
    /** @tentative-return-type */ public function hasChildren(): bool {}
    /** @tentative-return-type */ public function getChildren(): ?RecursiveCachingIterator {}
}
class NoRewindIterator extends IteratorIterator {
    public function __construct(Iterator $iterator) {}
    /** @tentative-return-type */ public function rewind(): void {}
    /** @tentative-return-type */ public function valid(): bool {}
    /** @tentative-return-type */ public function key(): mixed {}
    /** @tentative-return-type */ public function current(): mixed {}
    /** @tentative-return-type */ public function next(): void {}
}
class AppendIterator extends IteratorIterator {
    public function __construct() {}
    /** @tentative-return-type */ public function append(Iterator $iterator): void {}
    /** @tentative-return-type */ public function rewind(): void {}
    /** @tentative-return-type */ public function valid(): bool {}
    /** @tentative-return-type */ public function current(): mixed {}
    /** @tentative-return-type */ public function next(): void {}
    /** @tentative-return-type */ public function getIteratorIndex(): ?int {}
    /** @tentative-return-type */ public function getArrayIterator(): ArrayIterator {}
}
class InfiniteIterator extends IteratorIterator {
    public function __construct(Iterator $iterator) {}
    /** @tentative-return-type */ public function next(): void {}
}
class EmptyIterator implements Iterator {
    /** @tentative-return-type */ public function current(): never {}
    /** @tentative-return-type */ public function next(): void {}
    /** @tentative-return-type */ public function key(): never {}
    /** @tentative-return-type */ public function valid(): false {}
    /** @tentative-return-type */ public function rewind(): void {}
}
class RecursiveTreeIterator extends RecursiveIteratorIterator {
    public const int BYPASS_CURRENT = 4;
    public const int BYPASS_KEY = 8;
    public const int PREFIX_LEFT = 0;
    public const int PREFIX_MID_HAS_NEXT = 1;
    public const int PREFIX_MID_LAST = 2;
    public const int PREFIX_END_HAS_NEXT = 3;
    public const int PREFIX_END_LAST = 4;
    public const int PREFIX_RIGHT = 5;
    public function __construct(RecursiveIterator|IteratorAggregate $iterator, int $flags = RecursiveTreeIterator::BYPASS_KEY, int $cachingIteratorFlags = CachingIterator::CATCH_GET_CHILD, int $mode = RecursiveTreeIterator::SELF_FIRST) {}
    /** @tentative-return-type */ public function key(): mixed {}
    /** @tentative-return-type */ public function current(): mixed {}
    /** @tentative-return-type */ public function getPrefix(): string {}
    /** @tentative-return-type */ public function setPostfix(string $postfix): void {}
    /** @tentative-return-type */ public function setPrefixPart(int $part, string $value): void {}
    /** @tentative-return-type */ public function getEntry(): string {}
    /** @tentative-return-type */ public function getPostfix(): string {}
}
";

/// Which payload a class of this file carries: the recursive
/// iterators', the outer iterators', or none for an interface and
/// `EmptyIterator`.
pub(crate) enum Payload {
    None,
    Dual,
    Rec,
}

/// The interfaces and classes in `PHP_MINIT_FUNCTION(spl_iterators)`'s
/// order, each method with its entry, and the payload each carries.
///
/// ```php
/// $it = new LimitIterator(new ArrayIterator([1, 2, 3, 4, 5, 6]), 2, 3);
/// foreach ($it as $k => $v) echo "$k:$v;";
/// echo "\n";
/// $even = new CallbackFilterIterator(new ArrayIterator([1, 2, 3, 4]), fn ($v) => $v % 2 == 0);
/// echo implode(',', iterator_to_array($even, false)), "\n";
/// $all = new AppendIterator; $all->append(new ArrayIterator([1, 2])); $all->append(new ArrayIterator([3]));
/// echo implode(',', iterator_to_array($all, false)), "\n";
/// $tree = new RecursiveIteratorIterator(new RecursiveArrayIterator([1, [2, [3]], 4]), RecursiveIteratorIterator::SELF_FIRST);
/// foreach ($tree as $k => $v) echo $tree->getDepth(), is_array($v) ? 'A' : $v, ";";
/// echo "\n";
/// ```
/// ```expect
/// 2:3;3:4;4:5;
/// 2,4
/// 1,2,3
/// 01;0A;12;1A;23;04;
/// ```
// One row per method of the stub, which is what the linter counts.
#[allow(clippy::too_many_lines)]
pub(crate) fn classes() -> Vec<(Declared, Payload)> {
    let rows: Vec<(&str, Entry)> = vec![
        ("RecursiveIteratorIterator::__construct", rii_construct),
        ("RecursiveIteratorIterator::rewind", rii_rewind),
        ("RecursiveIteratorIterator::valid", rii_valid),
        ("RecursiveIteratorIterator::key", rii_key),
        ("RecursiveIteratorIterator::current", rii_current),
        ("RecursiveIteratorIterator::next", rii_next),
        ("RecursiveIteratorIterator::getDepth", rii_get_depth),
        (
            "RecursiveIteratorIterator::getSubIterator",
            rii_get_sub_iterator,
        ),
        (
            "RecursiveIteratorIterator::getInnerIterator",
            rii_get_inner_iterator,
        ),
        ("RecursiveIteratorIterator::beginIteration", nothing),
        ("RecursiveIteratorIterator::endIteration", nothing),
        (
            "RecursiveIteratorIterator::callHasChildren",
            rii_call_has_children,
        ),
        (
            "RecursiveIteratorIterator::callGetChildren",
            rii_call_get_children,
        ),
        ("RecursiveIteratorIterator::beginChildren", nothing),
        ("RecursiveIteratorIterator::endChildren", nothing),
        ("RecursiveIteratorIterator::nextElement", nothing),
        ("RecursiveIteratorIterator::setMaxDepth", rii_set_max_depth),
        ("RecursiveIteratorIterator::getMaxDepth", rii_get_max_depth),
        ("IteratorIterator::__construct", ii_construct),
        ("IteratorIterator::getInnerIterator", ii_get_inner_iterator),
        ("IteratorIterator::rewind", ii_rewind),
        ("IteratorIterator::valid", ii_valid),
        ("IteratorIterator::key", ii_key),
        ("IteratorIterator::current", ii_current),
        ("IteratorIterator::next", ii_next),
        ("FilterIterator::__construct", filter_construct),
        ("FilterIterator::rewind", filter_rewind),
        ("FilterIterator::next", filter_next),
        (
            "RecursiveFilterIterator::__construct",
            recursive_filter_construct,
        ),
        (
            "RecursiveFilterIterator::hasChildren",
            recursive_filter_has_children,
        ),
        (
            "RecursiveFilterIterator::getChildren",
            recursive_filter_get_children,
        ),
        ("CallbackFilterIterator::__construct", callback_construct),
        ("CallbackFilterIterator::accept", callback_accept),
        (
            "RecursiveCallbackFilterIterator::__construct",
            recursive_callback_construct,
        ),
        (
            "RecursiveCallbackFilterIterator::hasChildren",
            recursive_filter_has_children,
        ),
        (
            "RecursiveCallbackFilterIterator::getChildren",
            recursive_callback_get_children,
        ),
        ("ParentIterator::__construct", recursive_filter_construct),
        ("ParentIterator::accept", recursive_filter_has_children),
        ("LimitIterator::__construct", limit_construct),
        ("LimitIterator::rewind", limit_rewind),
        ("LimitIterator::valid", limit_valid),
        ("LimitIterator::next", limit_next),
        ("LimitIterator::seek", limit_seek),
        ("LimitIterator::getPosition", limit_get_position),
        ("CachingIterator::__construct", caching_construct),
        ("CachingIterator::rewind", caching_rewind),
        ("CachingIterator::valid", caching_valid),
        ("CachingIterator::next", caching_next),
        ("CachingIterator::hasNext", caching_has_next),
        ("CachingIterator::__toString", caching_to_string),
        ("CachingIterator::getFlags", caching_get_flags),
        ("CachingIterator::setFlags", caching_set_flags),
        ("CachingIterator::offsetGet", caching_offset_get),
        ("CachingIterator::offsetSet", caching_offset_set),
        ("CachingIterator::offsetUnset", caching_offset_unset),
        ("CachingIterator::offsetExists", caching_offset_exists),
        ("CachingIterator::getCache", caching_get_cache),
        ("CachingIterator::count", caching_count),
        (
            "RecursiveCachingIterator::__construct",
            recursive_caching_construct,
        ),
        (
            "RecursiveCachingIterator::hasChildren",
            recursive_caching_has_children,
        ),
        (
            "RecursiveCachingIterator::getChildren",
            recursive_caching_get_children,
        ),
        ("NoRewindIterator::__construct", no_rewind_construct),
        ("NoRewindIterator::rewind", nothing),
        ("NoRewindIterator::valid", no_rewind_valid),
        ("NoRewindIterator::key", no_rewind_key),
        ("NoRewindIterator::current", no_rewind_current),
        ("NoRewindIterator::next", no_rewind_next),
        ("AppendIterator::__construct", append_construct),
        ("AppendIterator::append", append_append),
        ("AppendIterator::rewind", append_rewind),
        ("AppendIterator::valid", ii_valid),
        ("AppendIterator::current", append_current),
        ("AppendIterator::next", append_next),
        (
            "AppendIterator::getIteratorIndex",
            append_get_iterator_index,
        ),
        (
            "AppendIterator::getArrayIterator",
            append_get_array_iterator,
        ),
        ("InfiniteIterator::__construct", infinite_construct),
        ("InfiniteIterator::next", infinite_next),
        ("EmptyIterator::current", empty_current),
        ("EmptyIterator::next", nothing),
        ("EmptyIterator::key", empty_key),
        ("EmptyIterator::valid", empty_valid),
        ("EmptyIterator::rewind", nothing),
        ("RecursiveTreeIterator::__construct", rti_construct),
        ("RecursiveTreeIterator::key", rti_key),
        ("RecursiveTreeIterator::current", rti_current),
        ("RecursiveTreeIterator::getPrefix", rti_get_prefix),
        ("RecursiveTreeIterator::setPostfix", rti_set_postfix),
        ("RecursiveTreeIterator::setPrefixPart", rti_set_prefix_part),
        ("RecursiveTreeIterator::getEntry", rti_get_entry),
        ("RecursiveTreeIterator::getPostfix", rti_get_postfix),
    ];
    stub::classes(STUB, &rows)
        .into_iter()
        .map(|(decl, methods)| {
            let name = decl.name.0.as_ref();
            let payload = if name == b"RecursiveIteratorIterator" {
                Payload::Rec
            } else if name == b"IteratorIterator" {
                Payload::Dual
            } else {
                Payload::None
            };
            ((decl, methods), payload)
        })
        .collect()
}

/// A method php serves as nothing, `beginIteration` and its kin and
/// `EmptyIterator::next`.
fn nothing(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    Ok(Value::null())
}

// ---- the outer iterators, `spl_dual_it_*`

fn dual<R>(
    req: &mut Request,
    object: PhpObject,
    change: impl FnOnce(&mut DualIt) -> R,
) -> Result<R, Thrown> {
    if let Some(it) = object.native_mut::<DualIt>(req) {
        return Ok(change(it));
    }
    Err(php_ext::raising::fatal(
        req,
        "InternalError: an SPL outer iterator served on an object without its payload",
    ))
}

/// `SPL_FETCH_AND_CHECK_DUAL_IT`: the receiver, its constructor served.
fn this_dual(req: &mut Request) -> Result<PhpObject, Thrown> {
    let this = crate::this(req)?;
    if dual(req, this, |it| it.kind)? == Kind::Unknown {
        return Err(throw(
            req,
            "Error",
            "The object is in an invalid state as the parent constructor was not called",
        ));
    }
    Ok(this)
}

fn inner(req: &mut Request, this: PhpObject) -> Result<Option<PhpObject>, Thrown> {
    dual(req, this, |it| it.inner)
}

/// `spl_dual_it_free`: the fetched element and key given up, and a
/// caching iterator's string and children.
fn dual_free(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    let held = dual(req, this, |it| {
        let children = if matches!(it.kind, Kind::Caching | Kind::RecursiveCaching) {
            it.children.take()
        } else {
            None
        };
        (it.data.take(), it.key.take(), it.string.take(), children)
    })?;
    for value in [held.0, held.1, held.2].into_iter().flatten() {
        req.release(value);
    }
    if let Some(children) = held.3 {
        req.release(children.as_value());
    }
    Ok(())
}

/// `spl_dual_it_rewind`.
fn dual_rewind(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    dual_free(req, this)?;
    dual(req, this, |it| it.pos = 0)?;
    if let Some(inner) = inner(req, this)? {
        crate::effect(req, inner, b"rewind")?;
    }
    Ok(())
}

/// `spl_dual_it_valid`.
fn dual_valid(req: &mut Request, this: PhpObject) -> Result<bool, Thrown> {
    match inner(req, this)? {
        Some(inner) => crate::valid(req, inner),
        None => Ok(false),
    }
}

/// `spl_dual_it_fetch`: the inner iterator's current element and key,
/// after `valid()` where `check_more` asks; `false` where it had none.
fn dual_fetch(req: &mut Request, this: PhpObject, check_more: bool) -> Result<bool, Thrown> {
    dual_free(req, this)?;
    if check_more && !dual_valid(req, this)? {
        return Ok(false);
    }
    let Some(inner) = inner(req, this)? else {
        return Ok(false);
    };
    let data = crate::method(req, inner, b"current")?;
    let data = crate::owned(req, data);
    dual(req, this, |it| it.data = Some(data))?;
    let key = crate::method(req, inner, b"key")?;
    let key = crate::owned(req, key);
    dual(req, this, |it| it.key = Some(key))?;
    Ok(true)
}

/// `spl_dual_it_next`.
fn dual_next(req: &mut Request, this: PhpObject, free: bool) -> Result<(), Thrown> {
    if free {
        dual_free(req, this)?;
    }
    let Some(inner) = inner(req, this)? else {
        return Err(throw(
            req,
            "Error",
            "The inner constructor wasn't initialized with an iterator instance",
        ));
    };
    crate::effect(req, inner, b"next")?;
    dual(req, this, |it| it.pos += 1)?;
    Ok(())
}

/// The class a constructor names for its inner argument.
fn class_named(req: &mut Request, name: &[u8]) -> Result<Option<ClassId>, Thrown> {
    lookup_class(req, name)
}

/// `spl_dual_it_construct`'s common half: the kind set, the inner
/// iterator taken with one more owner, and php's `BadMethodCallException`
/// for a second call.
fn dual_construct(
    req: &mut Request,
    base: &str,
    inner_class: &str,
    min: usize,
    max: usize,
) -> Result<(PhpObject, PhpObject), Thrown> {
    let this = crate::this(req)?;
    if dual(req, this, |it| it.kind)? != Kind::Unknown {
        return Err(throw(
            req,
            "BadMethodCallException",
            &format!("{base}::getIterator() must be called exactly once per instance"),
        ));
    }
    crate::arity(req, min, max)?;
    let inner = crate::object(req, 1, Some(inner_class))?;
    Ok((this, inner))
}

fn dual_set(
    req: &mut Request,
    this: PhpObject,
    kind: Kind,
    inner: PhpObject,
) -> Result<(), Thrown> {
    req.addref(&inner.as_value());
    dual(req, this, |it| {
        it.kind = kind;
        it.inner = Some(inner);
    })
}

impl ObjectHandlers for DualHandlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let held = object.native_mut::<DualIt>(req).map(|it| {
            (
                [
                    it.data.take(),
                    it.key.take(),
                    it.string.take(),
                    it.callback_value.take(),
                ],
                [it.inner.take(), it.children.take(), it.appended.take()],
                it.cache.take(),
                it.callback.take(),
            )
        });
        if let Some((values, objects, cache, callback)) = held {
            for value in values.into_iter().flatten() {
                req.release(value);
            }
            for object in objects.into_iter().flatten() {
                req.release(object.as_value());
            }
            if let Some(cache) = cache {
                req.release(Value::array(cache));
            }
            let _ = callback;
        }
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(it) = object.native::<DualIt>(req) {
            for value in [it.data, it.key, it.string].into_iter().flatten() {
                visit(value);
            }
            for object in [it.inner, it.children, it.appended].into_iter().flatten() {
                visit(object.as_value());
            }
            if let Some(cache) = it.cache {
                visit(Value::array(cache));
            }
        }
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }
}

/// `IteratorIterator::__construct(Traversable $iterator, ?string $class = null)`:
/// an `IteratorAggregate` asked for its iterator.
///
/// ```php
/// $it = new IteratorIterator(new ArrayObject(['a' => 1, 'b' => 2]));
/// foreach ($it as $k => $v) echo "$k=$v;";
/// var_dump(get_class($it->getInnerIterator()), $it->valid(), $it->key(), $it->current());
/// $it->rewind(); var_dump($it->key()); $it->next(); $it->next(); var_dump($it->valid(), $it->current());
/// ```
/// ```expect
/// a=1;b=2;string(13) "ArrayIterator"
/// bool(false)
/// NULL
/// NULL
/// string(1) "a"
/// bool(false)
/// NULL
/// ```
fn ii_construct(req: &mut Request) -> Result<Value, Thrown> {
    let (this, given) = dual_construct(req, "IteratorIterator", "Traversable", 1, 2)?;
    let mut class = given.class();
    if !crate::is_a(req, class, b"Iterator")
        && let Some(name) = crate::nullable_string(req, 2)?
    {
        match class_named(req, name.as_bytes())? {
            Some(cast) if crate::class_is(req, class, cast, 0) => class = cast,
            _ => {
                return Err(throw(
                    req,
                    "LogicException",
                    "Class to downcast to not found or not base class or does not implement Traversable",
                ));
            }
        }
    }
    if !crate::is_a(req, given.class(), b"Iterator")
        && crate::is_a(req, class, b"IteratorAggregate")
    {
        let answered = call_method(req, &given, b"getIterator", &[])?;
        let Some(iterator) = answered
            .as_object()
            .copied()
            .filter(|o| crate::is_a(req, o.class(), b"Traversable"))
        else {
            req.release(answered);
            let name = String::from_utf8_lossy(&crate::class_name(req, class)).into_owned();
            return Err(throw(
                req,
                "LogicException",
                &format!("{name}::getIterator() must return an object that implements Traversable"),
            ));
        };
        dual(req, this, |it| {
            it.kind = Kind::IteratorIterator;
            it.inner = Some(iterator);
        })?;
        return Ok(Value::null());
    }
    dual_set(req, this, Kind::IteratorIterator, given)?;
    Ok(Value::null())
}

/// `getInnerIterator(): ?Iterator`.
fn ii_get_inner_iterator(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    Ok(match inner(req, this)? {
        Some(inner) => crate::copied(req, inner.as_value()),
        None => Value::null(),
    })
}

/// `IteratorIterator::rewind(): void`: the inner rewound and its first
/// element fetched.
fn ii_rewind(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    dual_rewind(req, this)?;
    dual_fetch(req, this, true)?;
    Ok(Value::null())
}

/// `valid(): bool`: whether an element was fetched.
fn ii_valid(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    Ok(Value::bool(dual(req, this, |it| it.data.is_some())?))
}

/// `key(): mixed`: the fetched key.
fn ii_key(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    let key = dual(req, this, |it| it.key)?;
    Ok(key.map_or_else(Value::null, |k| crate::copied(req, k)))
}

/// `current(): mixed`: the fetched element.
fn ii_current(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    let data = dual(req, this, |it| it.data)?;
    Ok(data.map_or_else(Value::null, |d| crate::copied(req, d)))
}

/// `IteratorIterator::next(): void`.
fn ii_next(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    dual_next(req, this, true)?;
    dual_fetch(req, this, true)?;
    Ok(Value::null())
}

/// `FilterIterator::__construct(Iterator $iterator)`.
///
/// ```php
/// class Odd extends FilterIterator {
///     public function accept(): bool { echo "accept(", $this->current(), ") "; return $this->current() % 2 == 1; }
/// }
/// foreach (new Odd(new ArrayIterator([1, 2, 3, 4, 5])) as $k => $v) echo "[$k=$v]";
/// echo "\n";
/// class Lazy extends FilterIterator { public function __construct() {} public function accept(): bool { return true; } }
/// try { (new Lazy)->rewind(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// accept(1) [0=1]accept(2) accept(3) [2=3]accept(4) accept(5) [4=5]
/// The object is in an invalid state as the parent constructor was not called
/// ```
fn filter_construct(req: &mut Request) -> Result<Value, Thrown> {
    let (this, inner) = dual_construct(req, "FilterIterator", "Iterator", 1, 1)?;
    dual_set(req, this, Kind::Default, inner)?;
    Ok(Value::null())
}

/// `spl_filter_it_fetch`: elements fetched until `accept()` answers
/// something true.
fn filter_fetch(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    while dual_fetch(req, this, true)? {
        let answered = call_method(req, &this, b"accept", &[])?;
        let accepted = truthy(req, &answered);
        req.release(answered);
        if accepted {
            return Ok(());
        }
        if let Some(inner) = inner(req, this)? {
            crate::effect(req, inner, b"next")?;
        }
    }
    dual_free(req, this)
}

/// `FilterIterator::rewind(): void`.
fn filter_rewind(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    dual_rewind(req, this)?;
    filter_fetch(req, this)?;
    Ok(Value::null())
}

/// `FilterIterator::next(): void`.
fn filter_next(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    dual_next(req, this, true)?;
    filter_fetch(req, this)?;
    Ok(Value::null())
}

/// `RecursiveFilterIterator::__construct(RecursiveIterator $iterator)`,
/// and `ParentIterator`'s.
///
/// ```php
/// $it = new RecursiveIteratorIterator(new ParentIterator(new RecursiveArrayIterator(['a' => [1, 'b' => [2]], 'c' => 3])), RecursiveIteratorIterator::SELF_FIRST);
/// foreach ($it as $k => $v) echo $k, ";";
/// ```
/// ```expect
/// a;b;
/// ```
fn recursive_filter_construct(req: &mut Request) -> Result<Value, Thrown> {
    let (this, inner) = dual_construct(req, "RecursiveFilterIterator", "RecursiveIterator", 1, 1)?;
    dual_set(req, this, Kind::Default, inner)?;
    Ok(Value::null())
}

/// `hasChildren(): bool`, the inner iterator's; `ParentIterator::accept`
/// too.
fn recursive_filter_has_children(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    match inner(req, this)? {
        Some(inner) => crate::method(req, inner, b"hasChildren"),
        None => Ok(Value::null()),
    }
}

/// `RecursiveFilterIterator::getChildren(): ?RecursiveFilterIterator`:
/// the inner iterator's children in a new object of this class.
fn recursive_filter_get_children(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    let Some(inner) = inner(req, this)? else {
        return Ok(Value::null());
    };
    let children = crate::method(req, inner, b"getChildren")?;
    let made = instantiate(req, this.class(), &[children]);
    req.release(children);
    Ok(made?.as_value())
}

/// `CallbackFilterIterator::__construct(Iterator $iterator, callable $callback)`.
///
/// ```php
/// $it = new CallbackFilterIterator(new ArrayIterator(['a' => 1, 'b' => 2, 'c' => 3]), function ($v, $k, $inner) {
///     echo "($v,$k,", get_class($inner), ")"; return $k != 'b';
/// });
/// foreach ($it as $k => $v) echo "[$k]";
/// ```
/// ```expect
/// (1,a,ArrayIterator)[a](2,b,ArrayIterator)(3,c,ArrayIterator)[c]
/// ```
fn callback_construct(req: &mut Request) -> Result<Value, Thrown> {
    callback_construct_as(
        req,
        Kind::CallbackFilter,
        "CallbackFilterIterator",
        "Iterator",
    )
}

fn callback_construct_as(
    req: &mut Request,
    kind: Kind,
    base: &str,
    inner_class: &str,
) -> Result<Value, Thrown> {
    let (this, inner) = dual_construct(req, base, inner_class, 2, 2)?;
    let given = crate::value(req, 2);
    let callback = match callable_checked(req, &given) {
        Ok(callback) => callback,
        Err(reason) => {
            return Err(arg_type_error(
                req,
                2,
                &format!("be a valid callback, {reason}"),
            ));
        }
    };
    req.addref(&given);
    dual_set(req, this, kind, inner)?;
    dual(req, this, |it| {
        it.callback = Some(callback);
        it.callback_value = Some(given);
    })?;
    Ok(Value::null())
}

/// `CallbackFilterIterator::accept(): bool`: the callback served with
/// the element, its key and the inner iterator.
fn callback_accept(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    let (data, key, inner, callback) = dual(req, this, |it| {
        (it.data, it.key, it.inner, it.callback.clone())
    })?;
    let (Some(data), Some(key), Some(callback)) = (data, key, callback) else {
        return Ok(Value::bool(false));
    };
    let inner = inner.map_or_else(Value::null, |o| o.as_value());
    let answered = call(req, &callback, &[data, key, inner])?;
    Ok(crate::owned(req, answered))
}

/// `RecursiveCallbackFilterIterator::__construct(RecursiveIterator $iterator, callable $callback)`.
///
/// ```php
/// $it = new RecursiveIteratorIterator(new RecursiveCallbackFilterIterator(
///     new RecursiveArrayIterator([1, [2, 3], 4, [5]]),
///     fn ($v, $k, $it) => $it->hasChildren() || $v > 2
/// ));
/// foreach ($it as $v) echo $v, ";";
/// ```
/// ```expect
/// 3;4;5;
/// ```
fn recursive_callback_construct(req: &mut Request) -> Result<Value, Thrown> {
    callback_construct_as(
        req,
        Kind::RecursiveCallbackFilter,
        "RecursiveCallbackFilterIterator",
        "RecursiveIterator",
    )
}

/// `RecursiveCallbackFilterIterator::getChildren()`: the children in a
/// new object of this class with the same callback.
fn recursive_callback_get_children(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    let (Some(inner), Some(callback)) = dual(req, this, |it| (it.inner, it.callback_value))? else {
        return Ok(Value::null());
    };
    let children = crate::method(req, inner, b"getChildren")?;
    let made = instantiate(req, this.class(), &[children, callback]);
    req.release(children);
    Ok(made?.as_value())
}

/// `LimitIterator::__construct(Iterator $iterator, int $offset = 0, int $limit = -1)`.
///
/// ```php
/// $it = new LimitIterator(new ArrayIterator(['a', 'b', 'c', 'd']), 1, 2);
/// foreach ($it as $k => $v) echo "$k=$v;";
/// var_dump($it->getPosition(), $it->seek(2));
/// try { $it->seek(0); } catch (OutOfBoundsException $e) { echo $e->getMessage(), "\n"; }
/// try { $it->seek(3); } catch (OutOfBoundsException $e) { echo $e->getMessage(), "\n"; }
/// try { new LimitIterator(new ArrayIterator([]), -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 1=b;2=c;int(3)
/// int(2)
/// Cannot seek to 0 which is below the offset 1
/// Cannot seek to 3 which is behind offset 1 plus count 2
/// LimitIterator::__construct(): Argument #2 ($offset) must be greater than or equal to 0
/// ```
fn limit_construct(req: &mut Request) -> Result<Value, Thrown> {
    let (this, inner) = dual_construct(req, "LimitIterator", "Iterator", 1, 3)?;
    let offset = crate::long_or(req, 2, 0)?;
    let count = crate::long_or(req, 3, -1)?;
    if offset < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    if count < -1 {
        return Err(arg_value_error(req, 3, "be greater than or equal to -1"));
    }
    dual_set(req, this, Kind::Limit, inner)?;
    dual(req, this, |it| {
        it.offset = offset;
        it.count = count;
    })?;
    Ok(Value::null())
}

/// `spl_limit_it_seek`.
fn limit_seek_to(req: &mut Request, this: PhpObject, pos: i64) -> Result<(), Thrown> {
    dual_free(req, this)?;
    let (offset, count, current) = dual(req, this, |it| (it.offset, it.count, it.pos))?;
    if pos < offset {
        return Err(throw(
            req,
            "OutOfBoundsException",
            &format!("Cannot seek to {pos} which is below the offset {offset}"),
        ));
    }
    if count != -1 && pos - offset >= count {
        return Err(throw(
            req,
            "OutOfBoundsException",
            &format!("Cannot seek to {pos} which is behind offset {offset} plus count {count}"),
        ));
    }
    let inner = inner(req, this)?;
    if pos != current
        && let Some(inner) = inner
        && crate::is_a(req, inner.class(), b"SeekableIterator")
    {
        let answered = call_method(req, &inner, b"seek", &[Value::int(pos)])?;
        req.release(answered);
        dual(req, this, |it| it.pos = pos)?;
        if limit_in_range(req, this)? && dual_valid(req, this)? {
            dual_fetch(req, this, false)?;
        }
        return Ok(());
    }
    if pos < current {
        dual_rewind(req, this)?;
    }
    while pos > dual(req, this, |it| it.pos)? && dual_valid(req, this)? {
        dual_next(req, this, true)?;
    }
    if dual_valid(req, this)? {
        dual_fetch(req, this, true)?;
    }
    Ok(())
}

fn limit_in_range(req: &mut Request, this: PhpObject) -> Result<bool, Thrown> {
    dual(req, this, |it| {
        it.count == -1 || it.pos - it.offset < it.count
    })
}

/// `LimitIterator::rewind(): void`.
fn limit_rewind(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    dual_rewind(req, this)?;
    let offset = dual(req, this, |it| it.offset)?;
    limit_seek_to(req, this, offset)?;
    Ok(Value::null())
}

/// `LimitIterator::valid(): bool`.
fn limit_valid(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    let fetched = dual(req, this, |it| it.data.is_some())?;
    Ok(Value::bool(limit_in_range(req, this)? && fetched))
}

/// `LimitIterator::next(): void`.
fn limit_next(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    dual_next(req, this, true)?;
    if limit_in_range(req, this)? {
        dual_fetch(req, this, true)?;
    }
    Ok(Value::null())
}

/// `LimitIterator::seek(int $offset): int`.
fn limit_seek(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this_dual(req)?;
    let pos = crate::long(req, 1)?;
    limit_seek_to(req, this, pos)?;
    Ok(Value::int(dual(req, this, |it| it.pos)?))
}

/// `LimitIterator::getPosition(): int`.
fn limit_get_position(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    Ok(Value::int(dual(req, this, |it| it.pos)?))
}

/// `spl_cit_check_flags`: at most one of the string flags.
fn caching_flags_ok(flags: i64) -> bool {
    [
        CIT_CALL_TOSTRING,
        CIT_TOSTRING_USE_KEY,
        CIT_TOSTRING_USE_CURRENT,
        CIT_TOSTRING_USE_INNER,
    ]
    .iter()
    .filter(|f| flags & **f != 0)
    .count()
        <= 1
}

const CIT_FLAGS_MESSAGE: &str = "contain only one of CachingIterator::CALL_TOSTRING, CachingIterator::TOSTRING_USE_KEY, CachingIterator::TOSTRING_USE_CURRENT, or CachingIterator::TOSTRING_USE_INNER";

/// `CachingIterator::__construct(Iterator $iterator, int $flags = CachingIterator::CALL_TOSTRING)`.
///
/// ```php
/// $it = new CachingIterator(new ArrayIterator(['a', 'b', 'c']));
/// foreach ($it as $v) echo $v, $it->hasNext() ? ',' : "\n";
/// echo $it, "\n";
/// $full = new CachingIterator(new ArrayIterator(['x' => 1, 'y' => 2]), CachingIterator::FULL_CACHE);
/// foreach ($full as $v) {}
/// var_dump($full->getCache(), count($full), $full['x'], isset($full['z']));
/// try { $it['a']; } catch (BadMethodCallException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// a,b,c
///
/// array(2) {
///   ["x"]=>
///   int(1)
///   ["y"]=>
///   int(2)
/// }
/// int(2)
/// int(1)
/// bool(false)
/// CachingIterator does not use a full cache (see CachingIterator::__construct)
/// ```
fn caching_construct(req: &mut Request) -> Result<Value, Thrown> {
    caching_construct_as(req, Kind::Caching, "CachingIterator", "Iterator")
}

fn caching_construct_as(
    req: &mut Request,
    kind: Kind,
    base: &str,
    inner_class: &str,
) -> Result<Value, Thrown> {
    let (this, inner) = dual_construct(req, base, inner_class, 1, 2)?;
    let flags = crate::long_or(req, 2, CIT_CALL_TOSTRING)?;
    if !caching_flags_ok(flags) {
        return Err(arg_value_error(req, 2, CIT_FLAGS_MESSAGE));
    }
    let cache = PhpArray::new(req)?;
    dual_set(req, this, kind, inner)?;
    dual(req, this, |it| {
        it.flags |= flags & CIT_PUBLIC;
        it.cache = Some(cache);
    })?;
    Ok(Value::null())
}

/// `spl_caching_it_next`: one element fetched ahead, cached under
/// `FULL_CACHE`, its children made for a `RecursiveCachingIterator`,
/// its string taken for `__toString`, and the inner moved on.
fn caching_next_step(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    if !dual_fetch(req, this, true)? {
        dual(req, this, |it| it.flags &= !CIT_VALID)?;
        return Ok(());
    }
    let (flags, kind, key, data, inner) = dual(req, this, |it| {
        it.flags |= CIT_VALID;
        (it.flags, it.kind, it.key, it.data, it.inner)
    })?;
    if flags & CIT_FULL_CACHE != 0
        && let (Some(key), Some(data)) = (key, data)
    {
        let mut cache = dual(req, this, |it| it.cache)?.map_or_else(|| PhpArray::new(req), Ok)?;
        crate::set_key(req, &mut cache, &key, crate::copied(req, data))?;
        dual(req, this, |it| it.cache = Some(cache))?;
    }
    if kind == Kind::RecursiveCaching
        && let Some(inner) = inner
    {
        let children = (|| -> Result<Option<PhpObject>, Thrown> {
            let answered = crate::method(req, inner, b"hasChildren")?;
            let has = truthy(req, &answered);
            req.release(answered);
            if !has {
                return Ok(None);
            }
            let children = crate::method(req, inner, b"getChildren")?;
            let Some(class) = lookup_class(req, b"RecursiveCachingIterator")? else {
                req.release(children);
                return Ok(None);
            };
            let made = instantiate(req, class, &[children, Value::int(flags & CIT_PUBLIC)]);
            req.release(children);
            made.map(Some)
        })();
        match children {
            Ok(children) => dual(req, this, |it| it.children = children)?,
            Err(thrown) if flags & CIT_CATCH_GET_CHILD != 0 => {
                php_ext::raising::clear(req, thrown)?;
            }
            Err(thrown) => return Err(thrown),
        }
    }
    if flags & (CIT_TOSTRING_USE_INNER | CIT_CALL_TOSTRING) != 0 {
        let source = if flags & CIT_TOSTRING_USE_INNER != 0 {
            inner.map_or_else(Value::null, |o| o.as_value())
        } else {
            data.unwrap_or_default()
        };
        let string = to_string(req, &source)?;
        dual(req, this, |it| it.string = Some(Value::string(string)))?;
    }
    dual_next(req, this, false)
}

/// `CachingIterator::rewind(): void`.
fn caching_rewind(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    dual_rewind(req, this)?;
    let old = dual(req, this, |it| it.cache.take())?;
    if let Some(old) = old {
        req.release(Value::array(old));
    }
    let fresh = PhpArray::new(req)?;
    dual(req, this, |it| it.cache = Some(fresh))?;
    caching_next_step(req, this)?;
    Ok(Value::null())
}

/// `CachingIterator::valid(): bool`.
fn caching_valid(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    Ok(Value::bool(dual(req, this, |it| {
        it.flags & CIT_VALID != 0
    })?))
}

/// `CachingIterator::next(): void`.
fn caching_next(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    caching_next_step(req, this)?;
    Ok(Value::null())
}

/// `hasNext(): bool`: the inner iterator's `valid()`, one ahead.
fn caching_has_next(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    Ok(Value::bool(dual_valid(req, this)?))
}

/// `__toString(): string`, what the flags name.
fn caching_to_string(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    let (flags, key, data, string) = dual(req, this, |it| (it.flags, it.key, it.data, it.string))?;
    if flags
        & (CIT_CALL_TOSTRING
            | CIT_TOSTRING_USE_KEY
            | CIT_TOSTRING_USE_CURRENT
            | CIT_TOSTRING_USE_INNER)
        == 0
    {
        let class = String::from_utf8_lossy(&crate::class_name(req, this.class())).into_owned();
        return Err(throw(
            req,
            "BadMethodCallException",
            &format!("{class} does not fetch string value (see CachingIterator::__construct)"),
        ));
    }
    let chosen = if flags & CIT_TOSTRING_USE_KEY != 0 {
        key
    } else if flags & CIT_TOSTRING_USE_CURRENT != 0 {
        data
    } else {
        string
    };
    match chosen {
        Some(value) => Ok(Value::string(to_string(req, &value)?)),
        None => crate::string_value(req, b""),
    }
}

/// `getFlags(): int`.
fn caching_get_flags(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    Ok(Value::int(dual(req, this, |it| it.flags)?))
}

/// `setFlags(int $flags): void`.
///
/// ```php
/// $it = new CachingIterator(new ArrayIterator([1]));
/// try { $it->setFlags(0); } catch (InvalidArgumentException $e) { echo $e->getMessage(), "\n"; }
/// $it->setFlags(CachingIterator::CALL_TOSTRING | CachingIterator::FULL_CACHE);
/// var_dump($it->getFlags());
/// ```
/// ```expect
/// Unsetting flag CALL_TO_STRING is not possible
/// int(257)
/// ```
fn caching_set_flags(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this_dual(req)?;
    let flags = crate::long(req, 1)?;
    if !caching_flags_ok(flags) {
        return Err(arg_value_error(req, 1, CIT_FLAGS_MESSAGE));
    }
    let old = dual(req, this, |it| it.flags)?;
    if old & CIT_CALL_TOSTRING != 0 && flags & CIT_CALL_TOSTRING == 0 {
        return Err(throw(
            req,
            "InvalidArgumentException",
            "Unsetting flag CALL_TO_STRING is not possible",
        ));
    }
    if old & CIT_TOSTRING_USE_INNER != 0 && flags & CIT_TOSTRING_USE_INNER == 0 {
        return Err(throw(
            req,
            "InvalidArgumentException",
            "Unsetting flag TOSTRING_USE_INNER is not possible",
        ));
    }
    if flags & CIT_FULL_CACHE != 0 && old & CIT_FULL_CACHE == 0 {
        let fresh = PhpArray::new(req)?;
        if let Some(cache) = dual(req, this, |it| it.cache.replace(fresh))? {
            req.release(Value::array(cache));
        }
    }
    dual(req, this, |it| {
        it.flags = (it.flags & !CIT_PUBLIC) | (flags & CIT_PUBLIC);
    })?;
    Ok(Value::null())
}

/// The cache, or php's `BadMethodCallException` without `FULL_CACHE`.
fn full_cache(req: &mut Request, this: PhpObject) -> Result<PhpArray, Thrown> {
    let (flags, cache) = dual(req, this, |it| (it.flags, it.cache))?;
    match cache {
        Some(cache) if flags & CIT_FULL_CACHE != 0 => Ok(cache),
        _ => {
            let class = String::from_utf8_lossy(&crate::class_name(req, this.class())).into_owned();
            Err(throw(
                req,
                "BadMethodCallException",
                &format!("{class} does not use a full cache (see CachingIterator::__construct)"),
            ))
        }
    }
}

fn caching_key(req: &mut Request) -> Result<Key, Thrown> {
    Ok(Key::from_string(crate::string(req, 1)?))
}

/// `offsetGet($key): mixed`: the cached element, the warning for one
/// not cached.
fn caching_offset_get(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this_dual(req)?;
    let cache = full_cache(req, this)?;
    let key = caching_key(req)?;
    if let Some(value) = cache.get(&key) {
        return Ok(crate::copied(req, value));
    }
    let shown = match &key {
        Key::Int(n) => n.to_string(),
        Key::Str(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
    };
    php_ext::raising::warning(req, &format!("Undefined array key \"{shown}\""));
    Ok(Value::null())
}

/// `offsetSet($key, mixed $value): void`.
fn caching_offset_set(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 2, 2)?;
    let this = this_dual(req)?;
    let mut cache = full_cache(req, this)?;
    let key = caching_key(req)?;
    let value = crate::value(req, 2);
    cache.set(req, key, crate::copied(req, value))?;
    dual(req, this, |it| it.cache = Some(cache))?;
    Ok(Value::null())
}

/// `offsetUnset($key): void`.
fn caching_offset_unset(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this_dual(req)?;
    let mut cache = full_cache(req, this)?;
    let key = caching_key(req)?;
    cache.unset(req, &key)?;
    dual(req, this, |it| it.cache = Some(cache))?;
    Ok(Value::null())
}

/// `offsetExists($key): bool`.
fn caching_offset_exists(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this_dual(req)?;
    let cache = full_cache(req, this)?;
    let key = caching_key(req)?;
    Ok(Value::bool(cache.contains(&key)))
}

/// `getCache(): array`.
fn caching_get_cache(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    let cache = full_cache(req, this)?;
    Ok(crate::copied(req, Value::array(cache)))
}

/// `count(): int`, the cached elements.
fn caching_count(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    let cache = full_cache(req, this)?;
    Ok(Value::int(i64::try_from(cache.len()).unwrap_or(i64::MAX)))
}

/// `RecursiveCachingIterator::__construct(Iterator $iterator, int $flags = CALL_TOSTRING)`.
///
/// ```php
/// $it = new RecursiveIteratorIterator(new RecursiveCachingIterator(new RecursiveArrayIterator([1, [2, 3]]), 0), RecursiveIteratorIterator::SELF_FIRST);
/// foreach ($it as $k => $v) echo $k, is_array($v) ? 'A' : $v, ";";
/// ```
/// ```expect
/// 01;1A;02;13;
/// ```
fn recursive_caching_construct(req: &mut Request) -> Result<Value, Thrown> {
    caching_construct_as(
        req,
        Kind::RecursiveCaching,
        "RecursiveCachingIterator",
        "RecursiveIterator",
    )
}

/// `hasChildren(): bool`: whether children were made for the element.
fn recursive_caching_has_children(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    Ok(Value::bool(dual(req, this, |it| it.children.is_some())?))
}

/// `getChildren(): ?RecursiveCachingIterator`.
fn recursive_caching_get_children(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    let children = dual(req, this, |it| it.children)?;
    Ok(children.map_or_else(Value::null, |c| crate::copied(req, c.as_value())))
}

/// `NoRewindIterator::__construct(Iterator $iterator)`: the inner
/// iterator's own methods, `rewind()` left out.
///
/// ```php
/// $inner = new ArrayIterator([1, 2, 3]);
/// $it = new NoRewindIterator($inner);
/// foreach ($it as $v) { echo $v; break; }
/// $inner->next();
/// foreach ($it as $v) echo $v;
/// ```
/// ```expect
/// 123
/// ```
fn no_rewind_construct(req: &mut Request) -> Result<Value, Thrown> {
    let (this, inner) = dual_construct(req, "NoRewindIterator", "Iterator", 1, 1)?;
    dual_set(req, this, Kind::NoRewind, inner)?;
    Ok(Value::null())
}

fn no_rewind_forward(req: &mut Request, method: &[u8]) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    match inner(req, this)? {
        Some(inner) => {
            let answered = crate::method(req, inner, method)?;
            Ok(crate::owned(req, answered))
        }
        None => Ok(Value::null()),
    }
}

/// `NoRewindIterator::valid(): bool`.
fn no_rewind_valid(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    Ok(Value::bool(dual_valid(req, this)?))
}

/// `NoRewindIterator::key(): mixed`.
fn no_rewind_key(req: &mut Request) -> Result<Value, Thrown> {
    no_rewind_forward(req, b"key")
}

/// `NoRewindIterator::current(): mixed`.
fn no_rewind_current(req: &mut Request) -> Result<Value, Thrown> {
    no_rewind_forward(req, b"current")
}

/// `NoRewindIterator::next(): void`.
fn no_rewind_next(req: &mut Request) -> Result<Value, Thrown> {
    let answered = no_rewind_forward(req, b"next")?;
    req.release(answered);
    Ok(Value::null())
}

/// `InfiniteIterator::__construct(Iterator $iterator)`: past the end,
/// rewound.
///
/// ```php
/// $it = new InfiniteIterator(new ArrayIterator([1, 2]));
/// $n = 0; foreach ($it as $v) { echo $v; if (++$n == 5) break; }
/// ```
/// ```expect
/// 12121
/// ```
fn infinite_construct(req: &mut Request) -> Result<Value, Thrown> {
    let (this, inner) = dual_construct(req, "InfiniteIterator", "Iterator", 1, 1)?;
    dual_set(req, this, Kind::Infinite, inner)?;
    Ok(Value::null())
}

/// `InfiniteIterator::next(): void`.
fn infinite_next(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    dual_next(req, this, true)?;
    if dual_valid(req, this)? {
        dual_fetch(req, this, false)?;
    } else {
        dual_rewind(req, this)?;
        if dual_valid(req, this)? {
            dual_fetch(req, this, false)?;
        }
    }
    Ok(Value::null())
}

/// `AppendIterator::__construct()`: an `ArrayIterator` of the iterators.
///
/// ```php
/// $it = new AppendIterator;
/// $it->append(new ArrayIterator(['a' => 1]));
/// $it->append(new ArrayIterator([]));
/// $it->append(new ArrayIterator(['b' => 2, 'c' => 3]));
/// foreach ($it as $k => $v) echo "$k=$v@", $it->getIteratorIndex(), ";";
/// echo "\n", count($it->getArrayIterator()), "\n";
/// ```
/// ```expect
/// a=1@0;b=2@2;c=3@2;
/// 3
/// ```
fn append_construct(req: &mut Request) -> Result<Value, Thrown> {
    let this = crate::this(req)?;
    if dual(req, this, |it| it.kind)? != Kind::Unknown {
        return Err(throw(
            req,
            "BadMethodCallException",
            "AppendIterator::getIterator() must be called exactly once per instance",
        ));
    }
    crate::arity(req, 0, 0)?;
    let Some(class) = lookup_class(req, b"ArrayIterator")? else {
        return Ok(Value::null());
    };
    let list = instantiate(req, class, &[])?;
    dual(req, this, |it| {
        it.kind = Kind::Append;
        it.appended = Some(list);
    })?;
    Ok(Value::null())
}

fn appended(req: &mut Request, this: PhpObject) -> Result<PhpObject, Thrown> {
    match dual(req, this, |it| it.appended)? {
        Some(list) => Ok(list),
        None => Err(throw(
            req,
            "BadMethodCallException",
            "Classes derived from AppendIterator must call AppendIterator::__construct()",
        )),
    }
}

/// `spl_append_it_next_iterator`: the inner iterator given up and the
/// next one the list stands at taken and rewound.
fn append_next_iterator(req: &mut Request, this: PhpObject) -> Result<bool, Thrown> {
    dual_free(req, this)?;
    if let Some(old) = dual(req, this, |it| it.inner.take())? {
        req.release(old.as_value());
    }
    let list = appended(req, this)?;
    if !crate::valid(req, list)? {
        return Ok(false);
    }
    let current = crate::method(req, list, b"current")?;
    let Some(next) = current.as_object().copied() else {
        req.release(current);
        return Ok(false);
    };
    dual(req, this, |it| it.inner = Some(next))?;
    dual_rewind(req, this)?;
    Ok(true)
}

/// `spl_append_it_fetch`.
fn append_fetch(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    while !dual_valid(req, this)? {
        let list = appended(req, this)?;
        crate::effect(req, list, b"next")?;
        if !append_next_iterator(req, this)? {
            return Ok(());
        }
    }
    dual_fetch(req, this, false)?;
    Ok(())
}

/// `append(Iterator $iterator): void`.
fn append_append(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = this_dual(req)?;
    let iterator = crate::object(req, 1, Some("Iterator"))?;
    let list = appended(req, this)?;
    let list_valid = crate::valid(req, list)?;
    let inner_valid = dual_valid(req, this)?;
    let answered = call_method(req, &list, b"append", &[iterator.as_value()])?;
    req.release(answered);
    if list_valid && !inner_valid {
        crate::effect(req, list, b"next")?;
    }
    if inner(req, this)?.is_none() || !dual_valid(req, this)? {
        if !crate::valid(req, list)? {
            crate::effect(req, list, b"rewind")?;
        }
        loop {
            let moved = append_next_iterator(req, this)?;
            if !moved || inner(req, this)? == Some(iterator) {
                break;
            }
        }
        append_fetch(req, this)?;
    }
    Ok(Value::null())
}

/// `AppendIterator::rewind(): void`.
fn append_rewind(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    let list = appended(req, this)?;
    crate::effect(req, list, b"rewind")?;
    if append_next_iterator(req, this)? {
        append_fetch(req, this)?;
    }
    Ok(Value::null())
}

/// `AppendIterator::current(): mixed`: fetched again.
fn append_current(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    dual_fetch(req, this, true)?;
    let data = dual(req, this, |it| it.data)?;
    Ok(data.map_or_else(Value::null, |d| crate::copied(req, d)))
}

/// `AppendIterator::next(): void`.
fn append_next(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    if dual_valid(req, this)? {
        dual_next(req, this, true)?;
    }
    append_fetch(req, this)?;
    Ok(Value::null())
}

/// `getIteratorIndex(): ?int`, the list's key.
fn append_get_iterator_index(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    let list = appended(req, this)?;
    crate::method(req, list, b"key")
}

/// `getArrayIterator(): ArrayIterator`.
fn append_get_array_iterator(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_dual(req)?;
    let list = appended(req, this)?;
    Ok(crate::copied(req, list.as_value()))
}

/// `EmptyIterator::current(): never`.
///
/// ```php
/// $e = new EmptyIterator;
/// foreach ($e as $v) echo "never";
/// var_dump($e->valid(), iterator_count($e));
/// try { $e->current(); } catch (BadMethodCallException $x) { echo $x->getMessage(), "\n"; }
/// try { $e->key(); } catch (BadMethodCallException $x) { echo $x->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// int(0)
/// Accessing the value of an EmptyIterator
/// Accessing the key of an EmptyIterator
/// ```
fn empty_current(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    Err(throw(
        req,
        "BadMethodCallException",
        "Accessing the value of an EmptyIterator",
    ))
}

/// `EmptyIterator::key(): never`.
fn empty_key(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    Err(throw(
        req,
        "BadMethodCallException",
        "Accessing the key of an EmptyIterator",
    ))
}

/// `EmptyIterator::valid(): false`.
fn empty_valid(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    Ok(Value::bool(false))
}

// ---- the recursive iterators, `spl_recursive_it_*`

fn rec<R>(
    req: &mut Request,
    object: PhpObject,
    change: impl FnOnce(&mut RecIt) -> R,
) -> Result<R, Thrown> {
    if let Some(it) = object.native_mut::<RecIt>(req) {
        return Ok(change(it));
    }
    Err(php_ext::raising::fatal(
        req,
        "InternalError: an SPL recursive iterator served on an object without its payload",
    ))
}

/// The receiver, php's `Error` where its constructor was not served.
fn this_rec(req: &mut Request) -> Result<PhpObject, Thrown> {
    let this = crate::this(req)?;
    if rec(req, this, |it| it.levels.is_empty())? {
        return Err(throw(
            req,
            "Error",
            "The object is in an invalid state as the parent constructor was not called",
        ));
    }
    Ok(this)
}

fn top(req: &mut Request, this: PhpObject) -> Result<Option<Level>, Thrown> {
    rec(req, this, |it| it.levels.last().copied())
}

fn set_state(req: &mut Request, this: PhpObject, state: State) -> Result<(), Thrown> {
    rec(req, this, |it| {
        if let Some(level) = it.levels.last_mut() {
            level.state = state;
        }
    })
}

/// A method of `RecursiveIteratorIterator` the walk calls where a user class overrides it,
/// `intern->beginIteration` and its kin, served on the receiver.
fn hook(req: &mut Request, this: PhpObject, lowered: &[u8], name: &[u8]) -> Result<bool, Thrown> {
    if !crate::overridden(req, this, lowered) {
        return Ok(false);
    }
    crate::effect(req, this, name)?;
    Ok(true)
}

/// An error a step raises, cleared under `CATCH_GET_CHILD` and handed
/// on otherwise.
fn caught(req: &mut Request, this: PhpObject, thrown: Thrown) -> Result<(), Thrown> {
    if rec(req, this, |it| it.flags & CIT_CATCH_GET_CHILD != 0)? {
        php_ext::raising::clear(req, thrown)
    } else {
        Err(thrown)
    }
}

/// `spl_recursive_it_move_forward_ex`.
// `spl_recursive_it_move_forward_ex`'s state machine, one arm per state,
// kept whole as php keeps it.
#[allow(clippy::too_many_lines)]
fn rec_forward(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    loop {
        let Some(level) = top(req, this)? else {
            return Ok(());
        };
        let iterator = level.iterator;
        let mut state = level.state;
        if state == State::Next {
            if let Err(thrown) = crate::effect(req, iterator, b"next") {
                caught(req, this, thrown)?;
            }
            state = State::Start;
        }
        let mut ended = false;
        if state == State::Start {
            let depth = rec(req, this, |it| it.levels.len())?;
            let valid = crate::valid(req, iterator)?;
            if rec(req, this, |it| it.levels.len())? != depth
                || top(req, this)?.map(|l| l.iterator) != Some(iterator)
            {
                return Ok(());
            }
            if valid {
                state = State::Test;
            } else {
                ended = true;
            }
        }
        if !ended {
            match state {
                State::Test => {
                    set_state(req, this, State::Test)?;
                    let answered = if crate::overridden(req, this, b"callhaschildren") {
                        call_method(req, &this, b"callHasChildren", &[])
                    } else {
                        crate::method(req, iterator, b"hasChildren")
                    };
                    let has_children = match answered {
                        Ok(answered) => {
                            let has = truthy(req, &answered);
                            req.release(answered);
                            has
                        }
                        Err(thrown) => {
                            set_state(req, this, State::Next)?;
                            caught(req, this, thrown)?;
                            false
                        }
                    };
                    if has_children {
                        let (max_depth, depth, mode) =
                            rec(req, this, |it| (it.max_depth, it.levels.len(), it.mode))?;
                        let depth = i64::try_from(depth).unwrap_or(i64::MAX) - 1;
                        if max_depth == -1 || max_depth > depth {
                            let next = if mode == RIT_SELF_FIRST {
                                State::SelfFirst
                            } else {
                                State::Child
                            };
                            set_state(req, this, next)?;
                            continue;
                        }
                        if mode == RIT_LEAVES_ONLY {
                            set_state(req, this, State::Next)?;
                            continue;
                        }
                    }
                    hook(req, this, b"nextelement", b"nextElement")?;
                    set_state(req, this, State::Next)?;
                    return Ok(());
                }
                State::SelfFirst => {
                    let mode = rec(req, this, |it| it.mode)?;
                    if mode == RIT_SELF_FIRST || mode == RIT_CHILD_FIRST {
                        hook(req, this, b"nextelement", b"nextElement")?;
                    }
                    let next = if mode == RIT_SELF_FIRST {
                        State::Child
                    } else {
                        State::Next
                    };
                    set_state(req, this, next)?;
                    return Ok(());
                }
                State::Child => {
                    let child = if crate::overridden(req, this, b"callgetchildren") {
                        call_method(req, &this, b"callGetChildren", &[])
                    } else {
                        crate::method(req, iterator, b"getChildren")
                    };
                    let child = match child {
                        Ok(child) => child,
                        Err(thrown) => {
                            caught(req, this, thrown)?;
                            set_state(req, this, State::Next)?;
                            continue;
                        }
                    };
                    let Some(object) = child
                        .as_object()
                        .copied()
                        .filter(|o| crate::is_a(req, o.class(), b"RecursiveIterator"))
                    else {
                        req.release(child);
                        return Err(throw(
                            req,
                            "UnexpectedValueException",
                            "Objects returned by RecursiveIterator::getChildren() must implement RecursiveIterator",
                        ));
                    };
                    let mode = rec(req, this, |it| it.mode)?;
                    set_state(
                        req,
                        this,
                        if mode == RIT_CHILD_FIRST {
                            State::SelfFirst
                        } else {
                            State::Next
                        },
                    )?;
                    rec(req, this, |it| {
                        it.levels.push(Level {
                            iterator: object,
                            state: State::Start,
                        });
                    })?;
                    crate::effect(req, object, b"rewind")?;
                    if let Err(thrown) = hook(req, this, b"beginchildren", b"beginChildren") {
                        caught(req, this, thrown)?;
                    }
                    continue;
                }
                State::Next | State::Start => {}
            }
        }
        let depth = rec(req, this, |it| it.levels.len())?;
        if depth <= 1 {
            return Ok(());
        }
        if let Err(thrown) = hook(req, this, b"endchildren", b"endChildren") {
            caught(req, this, thrown)?;
        }
        let popped = rec(req, this, |it| {
            if it.levels.len() > 1 && it.levels.last().map(|l| l.iterator) == Some(iterator) {
                it.levels.pop()
            } else {
                None
            }
        })?;
        if let Some(level) = popped {
            req.release(level.iterator.as_value());
        }
    }
}

/// `spl_recursive_it_rewind_ex`: the stack cut back to the root,
/// `endChildren` for each level left, the root rewound, and
/// `beginIteration` once per walk.
fn rec_rewind(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    while rec(req, this, |it| it.levels.len())? > 1 {
        if let Some(level) = rec(req, this, |it| it.levels.pop())? {
            req.release(level.iterator.as_value());
        }
        hook(req, this, b"endchildren", b"endChildren")?;
    }
    set_state(req, this, State::Start)?;
    if let Some(root) = top(req, this)? {
        crate::effect(req, root.iterator, b"rewind")?;
    }
    if !rec(req, this, |it| it.in_iteration)? {
        hook(req, this, b"beginiteration", b"beginIteration")?;
    }
    rec(req, this, |it| it.in_iteration = true)?;
    rec_forward(req, this)
}

/// `spl_recursive_it_valid_ex`: any level valid, `endIteration` once
/// where none is.
fn rec_valid(req: &mut Request, this: PhpObject) -> Result<bool, Thrown> {
    let levels = rec(req, this, |it| it.levels.clone())?;
    for level in levels.iter().rev() {
        if crate::valid(req, level.iterator)? {
            return Ok(true);
        }
    }
    if rec(req, this, |it| it.in_iteration)? {
        hook(req, this, b"enditeration", b"endIteration")?;
    }
    rec(req, this, |it| it.in_iteration = false)?;
    Ok(false)
}

impl ObjectHandlers for RecHandlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let levels = object
            .native_mut::<RecIt>(req)
            .map(|it| std::mem::take(&mut it.levels))
            .unwrap_or_default();
        for level in levels {
            req.release(level.iterator.as_value());
        }
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(it) = object.native::<RecIt>(req) {
            for level in &it.levels {
                visit(level.iterator.as_value());
            }
        }
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }
}

/// `spl_recursive_it_it_construct`'s common half: the root taken, an
/// `IteratorAggregate` asked for its iterator, php's
/// `InvalidArgumentException` for anything not a `RecursiveIterator`.
fn rec_construct(
    req: &mut Request,
    this: PhpObject,
    given: PhpObject,
    mode: i64,
    flags: i64,
) -> Result<(), Thrown> {
    let root = if crate::is_a(req, given.class(), b"IteratorAggregate") {
        let answered = call_method(req, &given, b"getIterator", &[])?;
        match answered.as_object().copied() {
            Some(o) if crate::is_a(req, o.class(), b"Traversable") => o,
            _ => {
                req.release(answered);
                let name =
                    String::from_utf8_lossy(&crate::class_name(req, given.class())).into_owned();
                return Err(throw(
                    req,
                    "LogicException",
                    &format!(
                        "{name}::getIterator() must return an object that implements Traversable"
                    ),
                ));
            }
        }
    } else {
        req.addref(&given.as_value());
        given
    };
    if !crate::is_a(req, root.class(), b"RecursiveIterator") {
        req.release(root.as_value());
        return Err(throw(
            req,
            "InvalidArgumentException",
            "An instance of RecursiveIterator or IteratorAggregate creating it is required",
        ));
    }
    let old = rec(req, this, |it| {
        let old = std::mem::take(&mut it.levels);
        it.levels.push(Level {
            iterator: root,
            state: State::Start,
        });
        it.mode = mode;
        it.flags = flags;
        it.max_depth = -1;
        it.in_iteration = false;
        old
    })?;
    for level in old {
        req.release(level.iterator.as_value());
    }
    Ok(())
}

/// `RecursiveIteratorIterator::__construct()`: the `Traversable`, then
/// `LEAVES_ONLY` by default, then the flags, `0` by default.
///
/// ```php
/// class Loud extends RecursiveIteratorIterator {
///     function beginIteration(): void { echo "begin;"; }
///     function endIteration(): void { echo "end;"; }
///     function beginChildren(): void { echo "in;"; }
///     function endChildren(): void { echo "out;"; }
/// }
/// foreach (new Loud(new RecursiveArrayIterator([1, [2]])) as $k => $v) echo "$k=$v;";
/// echo "\n";
/// try { new RecursiveIteratorIterator(new ArrayIterator([])); } catch (InvalidArgumentException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// begin;0=1;in;0=2;out;end;
/// An instance of RecursiveIterator or IteratorAggregate creating it is required
/// ```
fn rii_construct(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 3)?;
    let this = crate::this(req)?;
    let given = crate::object(req, 1, Some("Traversable"))?;
    let mode = crate::long_or(req, 2, RIT_LEAVES_ONLY)?;
    let flags = crate::long_or(req, 3, 0)?;
    rec_construct(req, this, given, mode, flags)?;
    Ok(Value::null())
}

/// `rewind(): void`.
fn rii_rewind(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    rec_rewind(req, this)?;
    Ok(Value::null())
}

/// `valid(): bool`.
fn rii_valid(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    Ok(Value::bool(rec_valid(req, this)?))
}

/// `key(): mixed`, the sub-iterator's.
fn rii_key(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    let Some(level) = top(req, this)? else {
        return Ok(Value::null());
    };
    let key = crate::method(req, level.iterator, b"key")?;
    Ok(crate::owned(req, key))
}

/// `current(): mixed`, the sub-iterator's.
fn rii_current(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    let Some(level) = top(req, this)? else {
        return Ok(Value::null());
    };
    let current = crate::method(req, level.iterator, b"current")?;
    Ok(crate::owned(req, current))
}

/// `next(): void`.
fn rii_next(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    rec_forward(req, this)?;
    Ok(Value::null())
}

/// `getDepth(): int`.
///
/// ```php
/// $it = new RecursiveIteratorIterator(new RecursiveArrayIterator([[['x']]]));
/// foreach ($it as $v) { var_dump($it->getDepth(), $it->getSubIterator(0)->key(), get_class($it->getInnerIterator()), $it->getSubIterator(9)); }
/// $it->setMaxDepth(1); var_dump($it->getMaxDepth(), iterator_to_array($it)); $it->setMaxDepth(); var_dump($it->getMaxDepth());
/// ```
/// ```expect
/// int(2)
/// int(0)
/// string(22) "RecursiveArrayIterator"
/// NULL
/// int(1)
/// array(0) {
/// }
/// bool(false)
/// ```
fn rii_get_depth(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    let depth = rec(req, this, |it| it.levels.len())?;
    Ok(Value::int(i64::try_from(depth).unwrap_or(i64::MAX) - 1))
}

/// `getSubIterator(?int $level = null): ?RecursiveIterator`.
fn rii_get_sub_iterator(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    let this = this_rec(req)?;
    let levels = rec(req, this, |it| it.levels.clone())?;
    let wanted = match crate::given(req, 1).map(|v| v.deref()) {
        Some(v) if !matches!(v.as_ref(), ValueRef::Null) => crate::long(req, 1)?,
        _ => i64::try_from(levels.len()).unwrap_or(0) - 1,
    };
    let found = usize::try_from(wanted).ok().and_then(|at| levels.get(at));
    Ok(found.map_or_else(Value::null, |l| crate::copied(req, l.iterator.as_value())))
}

/// `getInnerIterator(): RecursiveIterator`, the current sub-iterator.
fn rii_get_inner_iterator(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    let level = top(req, this)?;
    Ok(level.map_or_else(Value::null, |l| crate::copied(req, l.iterator.as_value())))
}

/// `callHasChildren(): bool`, the sub-iterator's `hasChildren()`.
fn rii_call_has_children(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let Some(level) = top(req, this)? else {
        return Ok(Value::bool(false));
    };
    crate::method(req, level.iterator, b"hasChildren")
}

/// `callGetChildren(): ?RecursiveIterator`, the sub-iterator's
/// `getChildren()`.
fn rii_call_get_children(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    let Some(level) = top(req, this)? else {
        return Ok(Value::null());
    };
    crate::method(req, level.iterator, b"getChildren")
}

/// `setMaxDepth(int $maxDepth = -1): void`; the witness is `getDepth`'s.
fn rii_set_max_depth(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    let this = this_rec(req)?;
    let depth = crate::long_or(req, 1, -1)?;
    if depth < -1 {
        return Err(arg_value_error(req, 1, "be greater than or equal to -1"));
    }
    rec(req, this, |it| {
        it.max_depth = depth.min(i64::from(i32::MAX));
    })?;
    Ok(Value::null())
}

/// `getMaxDepth(): int|false`.
fn rii_get_max_depth(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    let depth = rec(req, this, |it| it.max_depth)?;
    Ok(if depth == -1 {
        Value::bool(false)
    } else {
        Value::int(depth)
    })
}

/// `RecursiveTreeIterator::__construct()`, over a `RecursiveIterator`
/// or an `IteratorAggregate`, with `BYPASS_KEY`, `CATCH_GET_CHILD` and
/// `SELF_FIRST` by default: the iterator wrapped in a `RecursiveCachingIterator`, which is what
/// `hasNext()` is asked of for the prefix.
///
/// ```php
/// $tree = new RecursiveTreeIterator(new RecursiveArrayIterator(['a' => ['b' => 1, 'c' => [2]], 'd' => 3]));
/// foreach ($tree as $k => $line) echo "[$k] $line\n";
/// $tree->setPrefixPart(RecursiveTreeIterator::PREFIX_LEFT, '>'); $tree->setPostfix('<');
/// $tree->rewind(); echo $tree->current(), "|", $tree->getPrefix(), "|", $tree->getEntry(), "|", $tree->getPostfix(), "\n";
/// ```
/// ```expect
/// [a] |-Array
/// [b] | |-1
/// [c] | \-Array
/// [0] |   \-2
/// [d] \-3
/// >|-Array<|>|-|Array|<
/// ```
fn rti_construct(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 4)?;
    let this = crate::this(req)?;
    let given = crate::value(req, 1);
    let Some(given) = given.as_object().copied() else {
        return Err(arg_type_error(
            req,
            1,
            "be of type RecursiveIterator|IteratorAggregate",
        ));
    };
    let flags = crate::long_or(req, 2, RTIT_BYPASS_KEY)?;
    let caching_flags = crate::long_or(req, 3, CIT_CATCH_GET_CHILD)?;
    let mode = crate::long_or(req, 4, RIT_SELF_FIRST)?;
    let source = if crate::is_a(req, given.class(), b"IteratorAggregate") {
        let answered = call_method(req, &given, b"getIterator", &[])?;
        match answered.as_object().copied() {
            Some(o) if crate::is_a(req, o.class(), b"Traversable") => o,
            _ => {
                req.release(answered);
                return Err(throw(
                    req,
                    "LogicException",
                    "getIterator() must return an object that implements Traversable",
                ));
            }
        }
    } else {
        req.addref(&given.as_value());
        given
    };
    let Some(class) = lookup_class(req, b"RecursiveCachingIterator")? else {
        req.release(source.as_value());
        return Ok(Value::null());
    };
    let caching = instantiate(req, class, &[source.as_value(), Value::int(caching_flags)]);
    req.release(source.as_value());
    let caching = caching?;
    let built = rec_construct(req, this, caching, mode, flags);
    req.release(caching.as_value());
    built?;
    rec(req, this, |it| {
        it.prefix = Some([
            Vec::new(),
            b"| ".to_vec(),
            b"  ".to_vec(),
            b"|-".to_vec(),
            b"\\-".to_vec(),
            Vec::new(),
        ]);
        it.postfix = Vec::new();
    })?;
    Ok(Value::null())
}

/// `spl_recursive_tree_iterator_get_prefix`.
fn tree_prefix(req: &mut Request, this: PhpObject) -> Result<Vec<u8>, Thrown> {
    let (levels, prefix) = rec(req, this, |it| (it.levels.clone(), it.prefix.clone()))?;
    let prefix = prefix.unwrap_or_default();
    let mut out = prefix[0].clone();
    let last = levels.len().saturating_sub(1);
    for (at, level) in levels.iter().enumerate() {
        let answered = crate::method(req, level.iterator, b"hasNext")?;
        let more = answered.as_bool() == Some(true);
        req.release(answered);
        let part = match (at == last, more) {
            (false, true) => 1,
            (false, false) => 2,
            (true, true) => 3,
            (true, false) => 4,
        };
        out.extend_from_slice(&prefix[part]);
    }
    out.extend_from_slice(&prefix[5]);
    Ok(out)
}

/// `spl_recursive_tree_iterator_get_entry`: the element as a string,
/// `Array` for an array.
fn tree_entry(req: &mut Request, this: PhpObject) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(level) = top(req, this)? else {
        return Ok(None);
    };
    let data = crate::method(req, level.iterator, b"current")?;
    let bare = data.deref();
    let entry = if bare.as_array().is_some() {
        b"Array".to_vec()
    } else {
        to_string(req, &bare)?.as_bytes().to_vec()
    };
    req.release(data);
    Ok(Some(entry))
}

/// `key(): mixed`: the key drawn with the prefix, or as it is under
/// `BYPASS_KEY`.
fn rti_key(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    let Some(level) = top(req, this)? else {
        return Ok(Value::null());
    };
    let key = crate::method(req, level.iterator, b"key")?;
    let key = crate::owned(req, key);
    if rec(req, this, |it| it.flags & RTIT_BYPASS_KEY != 0)? {
        return Ok(key);
    }
    let text = to_string(req, &key)?.as_bytes().to_vec();
    req.release(key);
    let mut out = tree_prefix(req, this)?;
    out.extend(text);
    out.extend(rec(req, this, |it| it.postfix.clone())?);
    crate::string_value(req, &out)
}

/// `current(): mixed`: the entry drawn with the prefix and postfix, or
/// the element under `BYPASS_CURRENT`.
fn rti_current(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    if rec(req, this, |it| it.flags & RTIT_BYPASS_CURRENT != 0)? {
        return rii_current(req);
    }
    let Some(entry) = tree_entry(req, this)? else {
        return Ok(Value::null());
    };
    let mut out = tree_prefix(req, this)?;
    out.extend(entry);
    out.extend(rec(req, this, |it| it.postfix.clone())?);
    crate::string_value(req, &out)
}

/// `getPrefix(): string`; the witness is the constructor's.
fn rti_get_prefix(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    let prefix = tree_prefix(req, this)?;
    crate::string_value(req, &prefix)
}

/// `setPostfix(string $postfix): void`.
fn rti_set_postfix(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let postfix = crate::string(req, 1)?.as_bytes().to_vec();
    rec(req, this, |it| it.postfix = postfix)?;
    Ok(Value::null())
}

/// `setPrefixPart(int $part, string $value): void`.
fn rti_set_prefix_part(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 2, 2)?;
    let this = crate::this(req)?;
    let part = crate::long(req, 1)?;
    let value = crate::string(req, 2)?.as_bytes().to_vec();
    let Some(part) = usize::try_from(part).ok().filter(|p| *p <= 5) else {
        return Err(arg_value_error(
            req,
            1,
            "be a RecursiveTreeIterator::PREFIX_* constant",
        ));
    };
    rec(req, this, |it| {
        if let Some(prefix) = it.prefix.as_mut() {
            prefix[part] = value;
        }
    })?;
    Ok(Value::null())
}

/// `getEntry(): string`.
fn rti_get_entry(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    match tree_entry(req, this)? {
        Some(entry) => crate::string_value(req, &entry),
        None => Ok(Value::null()),
    }
}

/// `getPostfix(): string`.
fn rti_get_postfix(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = this_rec(req)?;
    let postfix = rec(req, this, |it| it.postfix.clone())?;
    crate::string_value(req, &postfix)
}
