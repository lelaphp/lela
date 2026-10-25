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
//! `RecursiveRegexIterator` hold ext/pcre's compiled pattern
//! ([`Regex`]), php's `pcre_cache_entry`, and match with it through
//! the `php_pcre_*_impl` functions that crate exports, so a pattern is
//! compiled once, by ext/pcre, whichever extension asked.

use php_ext::abi::{
    Callable, Class, ClassIterator, ExtensionBuilder, False, FromArgument, GcTable, Instance,
    Never, ObjectHandlers, OrFalse, Raw, Refusals, Std, This, Union, Untyped,
};
use php_ext::attr::php_class;
use php_ext::call::{
    Function, Iter, autoload, call, call_method, instantiate, iterate, known_method, lookup_class,
    read_prop,
};
use php_ext::coerce::{coerce_param, to_string, truthy, with_string};
use php_ext::decl::{Name, Type};
use php_ext::raising::{
    Thrown, arg_type_error, arg_value_error, arity, throw, throw_error, throw_exception_ex,
    throwing_warnings,
};
use php_ext::request::Request;
use php_ext::values::{Emalloc, Key, Owner, PhpArray, PhpObject, PhpString, Value};
use php_ext_pcre::Regex;

use crate::array::ArrayIterator;
use crate::{ArrayAccess, Countable, Iterator, IteratorAggregate, Stringable, Traversable};

/// `CIT_PUBLIC`: the bits of a `CachingIterator`'s flags a program sets.
const CIT_PUBLIC: i64 = 0x0000_FFFF;
/// `CIT_VALID`: the element fetched ahead is there.
const CIT_VALID: i64 = 0x0001_0000;

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
    Regex,
    RecursiveRegex,
    CallbackFilter,
    RecursiveCallbackFilter,
}

/// `spl_dual_it_object`: each value it keeps is its owner, given up as
/// `spl_dual_it_free` and the object's free give php's up.
#[derive(Default)]
pub(crate) struct DualItState {
    kind: Kind,
    inner: Option<Owner<PhpObject>>,
    /// `inner.iterator`: the iterator the inner object's `get_iterator`
    /// handed out, which every step walks.
    iter: Option<Owner<Iter>>,
    data: Option<Owner<Value>>,
    key: Option<Owner<Value>>,
    pos: i64,
    offset: i64,
    count: i64,
    flags: i64,
    cache: Option<Owner<PhpArray>>,
    children: Option<Owner<PhpObject>>,
    string: Option<Owner<Value>>,
    /// `u.callback_filter`, the callable kept as `zend_fcc_addref` keeps
    /// it.
    callback: Option<Owner<Callable>>,
    appended: Option<Owner<PhpObject>>,
    regex: Option<RegexIt>,
}

/// The payload: `spl_dual_it_object`'s fields, boxed, inside the `sizeof` php
/// allocates (design §5.13).
pub(crate) type DualIt = Box<DualItState>;

/// `regex_mode`: what `RegexIterator::accept` makes of its subject,
/// each the `RegexIterator` constant of its number.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Match = 0,
    GetMatch = 1,
    AllMatches = 2,
    Split = 3,
    Replace = 4,
}

impl Mode {
    /// The `regex_mode` a number names, `None` from `REGIT_MODE_MAX` on and
    /// below zero, where php raises [`MODE_MESSAGE`].
    fn named(mode: i64) -> Option<Mode> {
        [
            Mode::Match,
            Mode::GetMatch,
            Mode::AllMatches,
            Mode::Split,
            Mode::Replace,
        ]
        .into_iter()
        .find(|m| m.number() == mode)
    }

    fn number(self) -> i64 {
        self as i64
    }
}

const MODE_MESSAGE: &str = "be RegexIterator::MATCH, RegexIterator::GET_MATCH, RegexIterator::ALL_MATCHES, RegexIterator::SPLIT, or RegexIterator::REPLACE";

/// `u.regex.flags`: the bits `setFlags` stored, every one kept as php
/// keeps it, of which `REGIT_USE_KEY` and `REGIT_INVERTED` are read.
#[derive(Clone, Copy)]
struct RegexFlags(i64);

impl RegexFlags {
    /// `REGIT_USE_KEY`: the key is the subject, and what `REPLACE` made
    /// replaces it.
    fn use_key(self) -> bool {
        self.0 & RegexIterator::USE_KEY != 0
    }

    /// `REGIT_INVERTED`: the answer turned over.
    fn inverted(self) -> bool {
        self.0 & RegexIterator::INVERT_MATCH != 0
    }
}

/// `spl_dual_it_object.u.regex`: the pattern out of ext/pcre's cache,
/// the string it was compiled from, and what `accept` runs it with.
#[derive(Clone)]
struct RegexIt {
    pattern: Regex,
    source: Box<[u8]>,
    mode: Mode,
    flags: RegexFlags,
    preg_flags: i64,
}

/// One level of `spl_recursive_it_object`'s stack, `spl_sub_iterator`:
/// the level's object, `zobject`, whose `hasChildren()` and
/// `getChildren()` the walk calls, and the iterator its class's
/// `get_iterator` handed out, `iterator`, which the walk steps; one owner
/// of each. The iterator is an object of the store (`zend_iterator_init`),
/// so it takes a number, and every number handed out after a walk that
/// went a level deep is php's.
///
/// ```php
/// class D extends RecursiveArrayIterator { function __destruct() { echo "dtor ", spl_object_id($this), "\n"; } }
/// foreach (new RecursiveIteratorIterator(new D([[1]])) as $v) {}
/// echo spl_object_id(new stdClass), "\n";
/// $t = new RecursiveTreeIterator(new RecursiveArrayIterator([1, [2, 3]]));
/// foreach ($t as $k => $v) echo "$k => $v\n";
/// echo spl_object_id(new stdClass), "\n";
/// ```
/// ```expect
/// dtor 5
/// dtor 2
/// 4
/// 0 => |-1
/// 1 => \-Array
/// 0 =>   |-2
/// 1 =>   \-3
/// 5
/// ```
struct Level {
    iterator: Owner<PhpObject>,
    sub: Owner<Iter>,
    state: State,
}

/// A level's views, copied off the stack for one step: what the step
/// reads while the level, which may be popped under it, holds the owners.
#[derive(Clone, Copy)]
struct Seen {
    iterator: PhpObject,
    sub: Iter,
    state: State,
}

impl Level {
    /// `ce->get_iterator(ce, &zobject, 0)` over `iterator`, whose owner
    /// the level takes.
    fn new(req: &mut Request, iterator: Owner<PhpObject>) -> Result<Level, Thrown> {
        match iterate(req, &iterator) {
            Ok(sub) => Ok(Level {
                iterator,
                sub,
                state: State::Start,
            }),
            Err(thrown) => {
                iterator.give_up(req);
                Err(thrown)
            }
        }
    }

    /// The level's views.
    fn seen(&self) -> Seen {
        Seen {
            iterator: *self.iterator,
            sub: *self.sub,
            state: self.state,
        }
    }

    /// `zend_iterator_dtor(sub_iter)` then `zval_ptr_dtor(&zobject)`, the
    /// order the rewind, the destructor and the free give a level up in.
    fn release(self, req: &mut Request) {
        self.sub.give_up(req);
        self.iterator.give_up(req);
    }
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
pub(crate) struct RecItState {
    levels: Vec<Level>,
    mode: i64,
    flags: i64,
    max_depth: i64,
    in_iteration: bool,
    /// `object->iterators`: the `spl_sub_iterator` array php allocates
    /// for the levels on the request's heap ([`fit_iterators`]).
    iterators: Option<Emalloc>,
    tree: Option<Tree>,
}

/// `sizeof(spl_sub_iterator)`: an iterator, its object, its class, its
/// state and the two methods php looks up once.
const SUB_ITERATOR: usize = 56;

/// `object->iterators` fitted to the levels standing, as php sizes it:
/// `emalloc`'d for the root, `erealloc`'d as a child is entered and as
/// the walk is cut back to its root, untouched as a child is left, and
/// given back with the last level
/// (`spl_RecursiveIteratorIterator_free_iterators`).
///
/// ```php
/// $i = new RecursiveArrayIterator([[[1]]]); $w = new RecursiveIteratorIterator($i);
/// $m = memory_get_usage(); $r = new RecursiveIteratorIterator($i); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 344
/// ```
fn fit_iterators(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    let (levels, held) = object
        .native_mut::<RecIt>(req)
        .map(|it| (it.levels.len(), it.iterators.take()))
        .unwrap_or_default();
    let bytes = levels * SUB_ITERATOR;
    let fitted = match held {
        Some(held) if bytes == 0 => {
            held.free(req);
            None
        }
        None if bytes == 0 => None,
        None => Some(Emalloc::new(req, bytes)?),
        Some(held) if held.len() == bytes => Some(held),
        Some(held) => Some(held.resize(req, bytes)?),
    };
    if let Some(it) = object.native_mut::<RecIt>(req) {
        it.iterators = fitted;
    }
    Ok(())
}

/// `prefix[6]` and `postfix[1]`: the strings a `RecursiveTreeIterator`
/// draws with, each with the one owner the object holds, as php's
/// `zend_string *` fields hold theirs: the constructor's
/// `ZSTR_INIT_LITERAL`s, strings of their own, and the empty string,
/// and what `setPrefixPart` and `setPostfix` were handed, one more
/// owner of it (design §5.13).
///
/// ```php
/// $i = new RecursiveArrayIterator([]); $w = new RecursiveTreeIterator($i);
/// $m = memory_get_usage(); $t = new RecursiveTreeIterator($i); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $t->setPrefixPart(RecursiveTreeIterator::PREFIX_LEFT, 'x'); $t->setPostfix('<>'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); unset($t); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 832
/// 0
/// -832
/// ```
struct Tree {
    /// The six parts, `PREFIX_LEFT` to `PREFIX_RIGHT`.
    prefix: Vec<Owner<PhpString>>,
    postfix: Owner<PhpString>,
}

impl Tree {
    /// The constructor's: `""`, `"| "`, `"  "`, `"|-"`, `"\\-"`, `""`,
    /// and the empty postfix.
    fn new(req: &mut Request) -> Result<Tree, Thrown> {
        let mut prefix = Vec::with_capacity(6);
        for text in [&b""[..], b"| ", b"  ", b"|-", b"\\-", b""] {
            let made = if text.is_empty() {
                PhpString::new(req, text)
            } else {
                PhpString::filled(req, text.len(), |bytes| bytes.copy_from_slice(text))
            };
            match made {
                Ok(part) => prefix.push(part),
                Err(thrown) => {
                    for part in prefix {
                        part.give_up(req);
                    }
                    return Err(thrown);
                }
            }
        }
        match PhpString::new(req, b"") {
            Ok(postfix) => Ok(Tree { prefix, postfix }),
            Err(thrown) => {
                for part in prefix {
                    part.give_up(req);
                }
                Err(thrown)
            }
        }
    }

    /// Each string's owner given up, as `spl_RecursiveIteratorIterator_free_storage`
    /// releases them.
    fn release(self, req: &mut Request) {
        for text in self.prefix.into_iter().chain([self.postfix]) {
            text.give_up(req);
        }
    }
}

/// The payload: `spl_recursive_it_object`'s fields, boxed, inside the `sizeof` php
/// allocates (design §5.13).
pub(crate) type RecIt = Box<RecItState>;

/// `spl_handlers_dual_it`.
pub(crate) struct DualHandlers;

pub(crate) static DUAL: DualHandlers = DualHandlers;

/// `spl_handlers_rec_it_it`.
pub(crate) struct RecHandlers;

pub(crate) static REC: RecHandlers = RecHandlers;

/// The interfaces and classes onto `builder`, in the order
/// `PHP_MINIT_FUNCTION(spl_iterators)` registers them.
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
pub(crate) fn register(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder
        .class::<RecursiveIterator>()
        .class::<OuterIterator>()
        .class::<RecursiveIteratorIterator>()
        .class::<IteratorIterator>()
        .class::<FilterIterator>()
        .class::<RecursiveFilterIterator>()
        .class::<CallbackFilterIterator>()
        .class::<RecursiveCallbackFilterIterator>()
        .class::<ParentIterator>()
        .class::<SeekableIterator>()
        .class::<LimitIterator>()
        .class::<CachingIterator>()
        .class::<RecursiveCachingIterator>()
        .class::<NoRewindIterator>()
        .class::<AppendIterator>()
        .class::<InfiniteIterator>()
        .class::<RegexIterator>()
        .class::<RecursiveRegexIterator>()
        .class::<EmptyIterator>()
        .class::<RecursiveTreeIterator>()
}

/// `interface RecursiveIterator extends Iterator`.
#[php_class(interface, extends(Iterator))]
pub(crate) struct RecursiveIterator;

#[php_class]
impl RecursiveIterator {
    #[php(tentative)]
    fn hasChildren(this: This<Self>) -> bool;
    #[php(tentative)]
    fn getChildren(this: This<Self>) -> Option<Instance<RecursiveIterator>>;
}

/// `interface OuterIterator extends Iterator`.
#[php_class(interface, extends(Iterator))]
pub(crate) struct OuterIterator;

#[php_class]
impl OuterIterator {
    #[php(tentative)]
    fn getInnerIterator(this: This<Self>) -> Option<Instance<Iterator>>;
}

// ---- `RecursiveIteratorIterator`, over `spl_recursive_it_object`

/// `class RecursiveIteratorIterator implements OuterIterator`: a walk
/// down a `RecursiveIterator`'s children, `spl_recursive_it_*`.
///
/// ```php
/// $it = new RecursiveIteratorIterator(new RecursiveArrayIterator([1, [2, [3]]]), RecursiveIteratorIterator::SELF_FIRST);
/// $it->rewind(); var_dump($it->valid(), $it->key(), $it->current(), $it->callHasChildren());
/// $it->next(); var_dump(get_class($it->callGetChildren()), get_class($it->getInnerIterator()));
/// $it->beginIteration(); $it->endIteration(); $it->beginChildren(); $it->endChildren(); $it->nextElement();
/// $it->setMaxDepth(0); var_dump($it->getMaxDepth(), $it->getSubIterator() === $it->getSubIterator(0));
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// int(1)
/// bool(false)
/// string(22) "RecursiveArrayIterator"
/// string(22) "RecursiveArrayIterator"
/// int(0)
/// bool(true)
/// ```
///
/// `zend_object_alloc(sizeof(spl_recursive_it_object))`, which
/// `memory_get_usage()` counts (design §5.13):
///
/// ```php
/// foreach ([false, true] as $shown) foreach (['RecursiveIteratorIterator'] as $c) {
///     $r = new ReflectionClass($c);
///     $m = memory_get_usage();
///     $o = $r->newInstanceWithoutConstructor();
///     $used = memory_get_usage() - $m;
///     $o = null;
///     if ($shown) echo $c, ' ', $used, "\n";
/// }
/// ```
/// ```expect
/// RecursiveIteratorIterator 192
/// ```
#[php_class(implements(OuterIterator), size = 208, payload = RecIt, handlers = REC)]
pub(crate) struct RecursiveIteratorIterator;

#[php_class]
impl RecursiveIteratorIterator {
    const LEAVES_ONLY: i64 = 0;
    const SELF_FIRST: i64 = 1;
    const CHILD_FIRST: i64 = 2;
    const CATCH_GET_CHILD: i64 = 16;

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
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Instance<Traversable>,
        #[php(default = RecursiveIteratorIterator::LEAVES_ONLY)] mode: i64,
        #[php(default = 0)] flags: i64,
    ) -> Result<(), Thrown> {
        rec_construct(req, this.object(), iterator.object(), mode, flags)
    }

    /// `rewind(): void`.
    #[php(tentative)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = rec_checked(req, this.object())?;
        rec_rewind(req, this)
    }

    /// `valid(): bool`.
    #[php(tentative)]
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = rec_checked(req, this.object())?;
        rec_valid(req, this)
    }

    /// `key(): mixed`, the sub-iterator's.
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<Owner<Value>, Thrown> {
        let this = rec_checked(req, this.object())?;
        rec_key(req, this)
    }

    /// `current(): mixed`, the sub-iterator's.
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<Owner<Value>, Thrown> {
        let this = rec_checked(req, this.object())?;
        rec_current(req, this)
    }

    /// `next(): void`.
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = rec_checked(req, this.object())?;
        rec_forward(req, this)
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
    #[php(tentative)]
    fn getDepth(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let this = rec_checked(req, this.object())?;
        let depth = rec(req, this, |it| it.levels.len())?;
        Ok(i64::try_from(depth).unwrap_or(i64::MAX) - 1)
    }

    /// `getSubIterator(?int $level = null): ?RecursiveIterator`; the
    /// witness is the class's.
    #[php(tentative)]
    fn getSubIterator(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] level: Option<i64>,
    ) -> Result<Option<Instance<RecursiveIterator>>, Thrown> {
        let this = rec_checked(req, this.object())?;
        let levels = seen_levels(req, this)?;
        let wanted = level.unwrap_or_else(|| i64::try_from(levels.len()).unwrap_or(0) - 1);
        let found = usize::try_from(wanted).ok().and_then(|at| levels.get(at));
        Ok(found.map(|l| Instance::of(l.iterator)))
    }

    /// `getInnerIterator(): RecursiveIterator`, the current sub-iterator.
    #[php(tentative)]
    fn getInnerIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Raw<Instance<RecursiveIterator>>>, Thrown> {
        let this = rec_checked(req, this.object())?;
        let level = top(req, this)?;
        let inner = match level {
            Some(l) => Owner::take(req, l.iterator.as_value()),
            None => Owner::null(),
        };
        Ok(crate::answered(inner))
    }

    /// `beginIteration(): void`, and `endIteration`, `beginChildren`,
    /// `endChildren` and `nextElement`: nothing, the walk serving them
    /// only where a class overrides them.
    #[php(tentative)]
    fn beginIteration(this: This<Self>) {
        let _ = this;
    }

    #[php(tentative, implementation = RecursiveIteratorIterator::beginIteration)]
    fn endIteration(this: This<Self>);

    /// `callHasChildren(): bool`, the sub-iterator's `hasChildren()`.
    #[php(tentative)]
    fn callHasChildren(req: &mut Request, this: This<Self>) -> Result<Owner<Raw<bool>>, Thrown> {
        let this = this.object();
        let answered = match top(req, this)? {
            Some(level) => crate::method(req, level.iterator, b"hasChildren")?,
            None => Owner::bool(false),
        };
        Ok(crate::answered(answered))
    }

    /// `callGetChildren(): ?RecursiveIterator`, the sub-iterator's
    /// `getChildren()`.
    #[php(tentative)]
    fn callGetChildren(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Raw<Option<Instance<RecursiveIterator>>>>, Thrown> {
        let this = rec_checked(req, this.object())?;
        let answered = match top(req, this)? {
            Some(level) => crate::method(req, level.iterator, b"getChildren")?,
            None => Owner::null(),
        };
        Ok(crate::answered(answered))
    }

    #[php(tentative, implementation = RecursiveIteratorIterator::beginIteration)]
    fn beginChildren(this: This<Self>);
    #[php(tentative, implementation = RecursiveIteratorIterator::beginIteration)]
    fn endChildren(this: This<Self>);
    #[php(tentative, implementation = RecursiveIteratorIterator::beginIteration)]
    fn nextElement(this: This<Self>);

    /// `setMaxDepth(int $maxDepth = -1): void`; the witness is the class's.
    #[php(tentative)]
    fn setMaxDepth(
        req: &mut Request,
        this: This<Self>,
        #[php(default = -1)] maxDepth: i64,
    ) -> Result<(), Thrown> {
        let this = rec_checked(req, this.object())?;
        if maxDepth < -1 {
            return Err(arg_value_error(req, 1, "be greater than or equal to -1"));
        }
        rec(req, this, |it| {
            it.max_depth = maxDepth.min(i64::from(i32::MAX));
        })
    }

    /// `getMaxDepth(): int|false`; the witness is the class's.
    #[php(tentative)]
    fn getMaxDepth(req: &mut Request, this: This<Self>) -> Result<OrFalse<i64>, Thrown> {
        let this = rec_checked(req, this.object())?;
        let depth = rec(req, this, |it| it.max_depth)?;
        Ok(if depth == -1 {
            OrFalse::False
        } else {
            OrFalse::Value(depth)
        })
    }
}

// ---- the outer iterators, over `spl_dual_it_object`

/// `class IteratorIterator implements OuterIterator`: an iterator over
/// any `Traversable`, an `IteratorAggregate` asked for its iterator.
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
///
/// `zend_object_alloc(sizeof(spl_dual_it_object))`, which
/// `memory_get_usage()` counts (design §5.13):
///
/// ```php
/// foreach ([false, true] as $shown) foreach (['IteratorIterator', 'CachingIterator', 'LimitIterator', 'AppendIterator'] as $c) {
///     $r = new ReflectionClass($c);
///     $m = memory_get_usage();
///     $o = $r->newInstanceWithoutConstructor();
///     $used = memory_get_usage() - $m;
///     $o = null;
///     if ($shown) echo $c, ' ', $used, "\n";
/// }
/// ```
/// ```expect
/// IteratorIterator 192
/// CachingIterator 192
/// LimitIterator 192
/// AppendIterator 192
/// ```
#[php_class(implements(OuterIterator), size = 192, payload = DualIt, handlers = DUAL)]
pub(crate) struct IteratorIterator;

#[php_class]
impl IteratorIterator {
    /// `IteratorIterator::__construct(Traversable $iterator, ?string $class = null)`:
    /// an `IteratorAggregate` asked for its iterator; a second call is
    /// php's `BadMethodCallException`, refused before the arguments are
    /// read. `$class` downcasts a non-`Iterator` to a base class of its
    /// own with an iterator, as `spl_dual_it_construct` sets `ce` to
    /// `ce_cast` before it asks for `zend_ce_aggregate`, so the
    /// `getIterator` asked is that class's, not the object's override.
    ///
    /// ```php
    /// class A extends ArrayObject { function getIterator(): Iterator { echo "A\n"; return parent::getIterator(); } }
    /// foreach (new IteratorIterator(new A([1, 2]), 'ArrayObject') as $v) echo $v, "\n";
    /// foreach (new IteratorIterator(new A([3])) as $v) echo $v, "\n";
    /// class B {} class C extends B implements IteratorAggregate { function getIterator(): Iterator { return new ArrayIterator([]); } }
    /// try { new IteratorIterator(new C, 'B'); } catch (LogicException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// 1
    /// 2
    /// A
    /// 3
    /// Class to downcast to not found or not base class or does not implement Traversable
    /// ```
    ///
    /// The class named is `zend_lookup_class`, which asks the autoloaders.
    ///
    /// ```php
    /// spl_autoload_register(function ($c) { echo "load $c\n"; });
    /// try { new IteratorIterator(new ArrayObject([1]), 'Nope'); } catch (LogicException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// load Nope
    /// Class to downcast to not found or not base class or does not implement Traversable
    /// ```
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Raw<Instance<Traversable>>,
        #[php(default = null)] class: Raw<Option<PhpString>>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        unconstructed(req, this, "IteratorIterator")?;
        arity(req, 1, Some(2))?;
        let given = iterator.hold(req)?.object();
        let mut cast = given.class();
        if !crate::is_a(req, cast, b"Iterator")
            && let Some(name) = class.hold(req)?
        {
            match autoload(req, name.as_bytes())? {
                Some(to)
                    if req.symbols().instance_of(cast, to)
                        && crate::is_a(req, to, b"Traversable") =>
                {
                    cast = to;
                }
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
            && crate::is_a(req, cast, b"IteratorAggregate")
        {
            let method = Name(b"getIterator".as_slice().into());
            let answered = match known_method(req, cast, &method, Some(given)) {
                Some(callable) => call(req, &callable, &[])?,
                None => call_method(req, &given, b"getIterator", &[])?,
            };
            let Some(held) = crate::object_answered(req, answered, b"Traversable") else {
                let name = String::from_utf8_lossy(&crate::class_name(req, cast)).into_owned();
                return Err(throw_exception_ex(
                    req,
                    "LogicException",
                    &format!(
                        "{name}::getIterator() must return an object that implements Traversable"
                    ),
                ));
            };
            let iterator = *held;
            let old = dual(req, this, |it| {
                it.kind = Kind::IteratorIterator;
                it.inner.replace(held)
            })?;
            crate::give_up(req, old);
            return inner_iterate(req, this, iterator);
        }
        dual_set(req, this, Kind::IteratorIterator, given)
    }

    /// `getInnerIterator(): ?Iterator`.
    #[php(tentative)]
    fn getInnerIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Instance<Iterator>>, Thrown> {
        let this = checked(req, this.object())?;
        Ok(inner(req, this)?.map(Instance::of))
    }

    /// `IteratorIterator::rewind(): void`: the inner rewound and its first
    /// element fetched.
    #[php(tentative)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        dual_rewind(req, this)?;
        dual_fetch(req, this, true)?;
        Ok(())
    }

    /// `valid(): bool`: whether an element was fetched.
    #[php(tentative)]
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = checked(req, this.object())?;
        dual(req, this, |it| it.data.is_some())
    }

    /// `key(): mixed`: the fetched key.
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let this = checked(req, this.object())?;
        let key = dual(req, this, |it| it.key.as_deref().copied())?;
        Ok(key.map_or_else(Value::null, |k| k.deref()))
    }

    /// `current(): mixed`: the fetched element.
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let this = checked(req, this.object())?;
        let data = dual(req, this, |it| it.data.as_deref().copied())?;
        Ok(data.map_or_else(Value::null, |d| d.deref()))
    }

    /// `IteratorIterator::next(): void`.
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        dual_next(req, this, true)?;
        dual_fetch(req, this, true)?;
        Ok(())
    }
}

/// `abstract class FilterIterator extends IteratorIterator`: the
/// elements its `accept()` answers true for.
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
#[php_class(abstract, extends(IteratorIterator))]
pub(crate) struct FilterIterator;

#[php_class]
impl FilterIterator {
    #[php(tentative)]
    fn accept(this: This<Self>) -> bool;

    /// `FilterIterator::__construct(Iterator $iterator)`.
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Raw<Instance<Iterator>>,
    ) -> Result<(), Thrown> {
        construct_inner(
            req,
            this.object(),
            "FilterIterator",
            iterator,
            Kind::Default,
        )
    }

    /// `FilterIterator::rewind(): void`.
    #[php(tentative)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        dual_rewind(req, this)?;
        filter_fetch(req, this)
    }

    /// `FilterIterator::next(): void`.
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        dual_next(req, this, true)?;
        filter_fetch(req, this)
    }
}

/// `abstract class RecursiveFilterIterator extends FilterIterator
/// implements RecursiveIterator`.
///
/// ```php
/// class Evens extends RecursiveFilterIterator { function accept(): bool { return $this->hasChildren() || $this->current() % 2 == 0; } }
/// $it = new Evens(new RecursiveArrayIterator([1, [2, 3], 4]));
/// foreach (new RecursiveIteratorIterator($it) as $v) echo $v, ";";
/// $it->rewind(); var_dump($it->hasChildren(), get_class($it->getChildren()));
/// ```
/// ```expect
/// 2;4;bool(true)
/// string(5) "Evens"
/// ```
#[php_class(abstract, extends(FilterIterator), implements(RecursiveIterator))]
pub(crate) struct RecursiveFilterIterator;

#[php_class]
impl RecursiveFilterIterator {
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
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Raw<Instance<RecursiveIterator>>,
    ) -> Result<(), Thrown> {
        construct_inner(
            req,
            this.object(),
            "RecursiveFilterIterator",
            iterator,
            Kind::Default,
        )
    }

    /// `hasChildren(): bool`, the inner iterator's; `ParentIterator::accept`
    /// and `RecursiveRegexIterator::hasChildren` too.
    ///
    /// ```php
    /// $it = new RecursiveRegexIterator(new RecursiveArrayIterator([['a'], 'a']), '/a/');
    /// foreach ($it as $v) var_dump($it->hasChildren());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn hasChildren(req: &mut Request, this: This<Self>) -> Result<Owner<Raw<bool>>, Thrown> {
        let this = checked(req, this.object())?;
        let answered = match inner(req, this)? {
            Some(inner) => crate::method(req, inner, b"hasChildren")?,
            None => Owner::null(),
        };
        Ok(crate::answered(answered))
    }

    /// `RecursiveFilterIterator::getChildren(): ?RecursiveFilterIterator`:
    /// the inner iterator's children in a new object of this class; the
    /// witness is the class's.
    #[php(tentative)]
    fn getChildren(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<RecursiveFilterIterator>>>, Thrown> {
        let this = checked(req, this.object())?;
        let Some(inner) = inner(req, this)? else {
            return Ok(None);
        };
        let children = crate::method(req, inner, b"getChildren")?;
        let made = instantiate(req, this.class(), &[*children]);
        children.give_up(req);
        Ok(Some(made?.into_instance()))
    }
}

/// `class CallbackFilterIterator extends FilterIterator`: the elements
/// a callback accepts.
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
#[php_class(extends(FilterIterator))]
pub(crate) struct CallbackFilterIterator;

#[php_class]
impl CallbackFilterIterator {
    /// `CallbackFilterIterator::__construct(Iterator $iterator, callable $callback)`.
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Raw<Instance<Iterator>>,
        callback: Raw<Callable>,
    ) -> Result<(), Thrown> {
        construct_callback(
            req,
            this.object(),
            "CallbackFilterIterator",
            iterator,
            callback,
            Kind::CallbackFilter,
        )
    }

    /// `CallbackFilterIterator::accept(): bool`: the callback served with
    /// the element, its key and the inner iterator, its answer handed back
    /// as it came.
    #[php(tentative)]
    fn accept(req: &mut Request, this: This<Self>) -> Result<Owner<Raw<bool>>, Thrown> {
        let this = checked(req, this.object())?;
        let (data, key, inner, callback) = dual(req, this, |it| {
            (
                it.data.as_deref().copied(),
                it.key.as_deref().copied(),
                it.inner.as_deref().copied(),
                it.callback.as_deref().cloned(),
            )
        })?;
        let (Some(data), Some(key), Some(callback)) = (data, key, callback) else {
            return Ok(crate::answered(Owner::bool(false)));
        };
        let inner = inner.map_or_else(Value::null, |o| o.as_value());
        let answered = call(req, &callback, &[data, key, inner])?;
        let bare = Owner::take(req, (*answered).deref());
        answered.give_up(req);
        Ok(crate::answered(bare))
    }
}

/// `class RecursiveCallbackFilterIterator extends CallbackFilterIterator
/// implements RecursiveIterator`.
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
#[php_class(extends(CallbackFilterIterator), implements(RecursiveIterator))]
pub(crate) struct RecursiveCallbackFilterIterator;

#[php_class]
impl RecursiveCallbackFilterIterator {
    /// `RecursiveCallbackFilterIterator::__construct(RecursiveIterator $iterator, callable $callback)`.
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Raw<Instance<RecursiveIterator>>,
        callback: Raw<Callable>,
    ) -> Result<(), Thrown> {
        construct_callback(
            req,
            this.object(),
            "RecursiveCallbackFilterIterator",
            iterator,
            callback,
            Kind::RecursiveCallbackFilter,
        )
    }

    #[php(tentative, implementation = RecursiveFilterIterator::hasChildren)]
    fn hasChildren(this: This<Self>) -> bool;

    /// `RecursiveCallbackFilterIterator::getChildren()`: the children in a
    /// new object of this class with the callback made again from what it
    /// resolved to ([`Callable::made_again`], which holds the witness).
    #[php(tentative)]
    fn getChildren(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<RecursiveCallbackFilterIterator>>, Thrown> {
        let this = checked(req, this.object())?;
        let (Some(inner), Some(callback)) = dual(req, this, |it| {
            (
                it.inner.as_deref().copied(),
                it.callback.as_deref().cloned(),
            )
        })?
        else {
            return Err(php_ext::raising::fatal(
                req,
                "InternalError: a RecursiveCallbackFilterIterator constructed without its inner iterator or callback",
            ));
        };
        let children = crate::method(req, inner, b"getChildren")?;
        let callback = match callback.made_again(req) {
            Ok(callback) => callback,
            Err(thrown) => {
                children.give_up(req);
                return Err(thrown);
            }
        };
        let made = instantiate(req, this.class(), &[*children, *callback]);
        callback.give_up(req);
        children.give_up(req);
        Ok(made?.into_instance())
    }
}

/// `class ParentIterator extends RecursiveFilterIterator`: the elements
/// that have children.
#[php_class(extends(RecursiveFilterIterator))]
pub(crate) struct ParentIterator;

#[php_class]
impl ParentIterator {
    #[php(implementation = RecursiveFilterIterator::__construct)]
    fn __construct(this: This<Self>, iterator: Instance<RecursiveIterator>);
    #[php(tentative, implementation = RecursiveFilterIterator::hasChildren)]
    fn accept(this: This<Self>) -> bool;
}

/// `interface SeekableIterator extends Iterator`.
///
/// ```php
/// $m = new ReflectionMethod("SeekableIterator", "seek");
/// var_dump($m->isAbstract(), $m->hasTentativeReturnType(), (string) $m->getTentativeReturnType(), $m->getParameters()[0]->getName());
/// var_dump((new ReflectionClass("OutOfRangeException"))->getParentClass()->getName(), (new ReflectionClass("RecursiveIterator"))->getInterfaceNames());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(4) "void"
/// string(6) "offset"
/// string(14) "LogicException"
/// array(2) {
///   [0]=>
///   string(8) "Iterator"
///   [1]=>
///   string(11) "Traversable"
/// }
/// ```
#[php_class(interface, extends(Iterator))]
pub(crate) struct SeekableIterator;

#[php_class]
impl SeekableIterator {
    #[php(tentative)]
    fn seek(this: This<Self>, offset: i64);
}

/// `class LimitIterator extends IteratorIterator`: a window of its
/// inner iterator's elements.
///
/// ```php
/// $it = new LimitIterator(new ArrayIterator(['a', 'b', 'c', 'd']), 1, 2);
/// foreach ($it as $k => $v) echo "$k=$v;";
/// var_dump($it->getPosition(), $it->seek(2));
/// try { $it->seek(0); } catch (OutOfBoundsException $e) { echo $e->getMessage(), "\n"; }
/// try { $it->seek(3); } catch (OutOfBoundsException $e) { echo $e->getMessage(), "\n"; }
/// try { new LimitIterator(new ArrayIterator([]), -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// $it->rewind(); $it->next(); var_dump($it->valid());
/// ```
/// ```expect
/// 1=b;2=c;int(3)
/// int(2)
/// Cannot seek to 0 which is below the offset 1
/// Cannot seek to 3 which is behind offset 1 plus count 2
/// LimitIterator::__construct(): Argument #2 ($offset) must be greater than or equal to 0
/// bool(true)
/// ```
#[php_class(extends(IteratorIterator))]
pub(crate) struct LimitIterator;

#[php_class]
impl LimitIterator {
    /// `LimitIterator::__construct(Iterator $iterator, int $offset = 0, int $limit = -1)`.
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Raw<Instance<Iterator>>,
        #[php(default = 0)] offset: Raw<i64>,
        #[php(default = -1)] limit: Raw<i64>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        unconstructed(req, this, "LimitIterator")?;
        arity(req, 1, Some(3))?;
        let inner = iterator.hold(req)?.object();
        let offset = offset.hold(req)?;
        let count = limit.hold(req)?;
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
        })
    }

    /// `LimitIterator::rewind(): void`.
    #[php(tentative)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        dual_rewind(req, this)?;
        let offset = dual(req, this, |it| it.offset)?;
        limit_seek_to(req, this, offset)
    }

    /// `LimitIterator::valid(): bool`.
    #[php(tentative)]
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = checked(req, this.object())?;
        let fetched = dual(req, this, |it| it.data.is_some())?;
        Ok(limit_in_range(req, this)? && fetched)
    }

    /// `LimitIterator::next(): void`.
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        dual_next(req, this, true)?;
        if limit_in_range(req, this)? {
            dual_fetch(req, this, true)?;
        }
        Ok(())
    }

    /// `LimitIterator::seek(int $offset): int`.
    #[php(tentative)]
    fn seek(req: &mut Request, this: This<Self>, offset: i64) -> Result<i64, Thrown> {
        let this = checked(req, this.object())?;
        limit_seek_to(req, this, offset)?;
        dual(req, this, |it| it.pos)
    }

    /// `LimitIterator::getPosition(): int`.
    #[php(tentative)]
    fn getPosition(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let this = checked(req, this.object())?;
        dual(req, this, |it| it.pos)
    }
}

/// `class CachingIterator extends IteratorIterator implements
/// ArrayAccess, Countable, Stringable`: one element fetched ahead, and
/// under `FULL_CACHE` every element kept.
///
/// ```php
/// $it = new CachingIterator(new ArrayIterator(['a', 'b', 'c']));
/// foreach ($it as $v) echo $v, $it->hasNext() ? ',' : "\n";
/// echo $it, "\n";
/// $full = new CachingIterator(new ArrayIterator(['x' => 1, 'y' => 2]), CachingIterator::FULL_CACHE);
/// foreach ($full as $v) {}
/// var_dump($full->getCache(), count($full), $full['x'], isset($full['z']));
/// try { $it['a']; } catch (BadMethodCallException $e) { echo $e->getMessage(), "\n"; }
/// try { new CachingIterator(new ArrayObject([])); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $full->offsetSet('z', 3); $full->offsetUnset('x'); var_dump($full->offsetExists('z'), $full->offsetGet('z'), $full->count(), $full->valid());
/// $it->rewind(); $it->next(); var_dump($it->valid(), $it->__toString(), $it->getFlags());
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
/// CachingIterator::__construct(): Argument #1 ($iterator) must be of type Iterator, ArrayObject given
/// bool(true)
/// int(3)
/// int(2)
/// bool(false)
/// bool(true)
/// string(1) "b"
/// int(65537)
/// ```
#[php_class(
    extends(IteratorIterator),
    implements(ArrayAccess, Countable, Stringable)
)]
pub(crate) struct CachingIterator;

#[php_class]
impl CachingIterator {
    const CALL_TOSTRING: i64 = 1;
    const CATCH_GET_CHILD: i64 = RecursiveIteratorIterator::CATCH_GET_CHILD;
    const TOSTRING_USE_KEY: i64 = 2;
    const TOSTRING_USE_CURRENT: i64 = 4;
    const TOSTRING_USE_INNER: i64 = 8;
    const FULL_CACHE: i64 = 256;

    /// `CachingIterator::__construct(Iterator $iterator, int $flags = CachingIterator::CALL_TOSTRING)`.
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Raw<Instance<Iterator>>,
        #[php(default = CachingIterator::CALL_TOSTRING)] flags: Raw<i64>,
    ) -> Result<(), Thrown> {
        construct_caching(
            req,
            this.object(),
            "CachingIterator",
            |req| Ok(iterator.hold(req)?.object()),
            flags,
            Kind::Caching,
        )
    }

    /// `CachingIterator::rewind(): void`.
    #[php(tentative)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        dual_rewind(req, this)?;
        let old = dual(req, this, |it| it.cache.take())?;
        crate::give_up(req, old);
        let fresh = PhpArray::new(req)?;
        let old = dual(req, this, |it| it.cache.replace(fresh))?;
        crate::give_up(req, old);
        caching_next_step(req, this)
    }

    /// `CachingIterator::valid(): bool`.
    #[php(tentative)]
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = checked(req, this.object())?;
        dual(req, this, |it| it.flags & CIT_VALID != 0)
    }

    /// `CachingIterator::next(): void`.
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        caching_next_step(req, this)
    }

    /// `hasNext(): bool`: the inner iterator's `valid()`, one ahead.
    #[php(tentative)]
    fn hasNext(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = checked(req, this.object())?;
        dual_valid(req, this)
    }

    /// `__toString(): string`, what the flags name.
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let this = checked(req, this.object())?;
        let (flags, key, data, string) = dual(req, this, |it| {
            (
                it.flags,
                it.key.as_deref().copied(),
                it.data.as_deref().copied(),
                it.string.as_deref().copied(),
            )
        })?;
        if flags
            & (Self::CALL_TOSTRING
                | Self::TOSTRING_USE_KEY
                | Self::TOSTRING_USE_CURRENT
                | Self::TOSTRING_USE_INNER)
            == 0
        {
            let class = String::from_utf8_lossy(&crate::class_name(req, this.class())).into_owned();
            return Err(throw_exception_ex(
                req,
                "BadMethodCallException",
                &format!("{class} does not fetch string value (see CachingIterator::__construct)"),
            ));
        }
        let chosen = if flags & Self::TOSTRING_USE_KEY != 0 {
            key
        } else if flags & Self::TOSTRING_USE_CURRENT != 0 {
            data
        } else {
            string
        };
        match chosen {
            Some(value) => to_string(req, &value),
            None => PhpString::new(req, b""),
        }
    }

    /// `getFlags(): int`; the witness is `setFlags`'s and the class's.
    #[php(tentative)]
    fn getFlags(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let this = checked(req, this.object())?;
        dual(req, this, |it| it.flags)
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
    #[php(tentative)]
    fn setFlags(req: &mut Request, this: This<Self>, flags: i64) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        if !caching_flags_ok(flags) {
            return Err(arg_value_error(req, 1, CIT_FLAGS_MESSAGE));
        }
        let old = dual(req, this, |it| it.flags)?;
        if old & Self::CALL_TOSTRING != 0 && flags & Self::CALL_TOSTRING == 0 {
            return Err(throw(
                req,
                "InvalidArgumentException",
                "Unsetting flag CALL_TO_STRING is not possible",
            ));
        }
        if old & Self::TOSTRING_USE_INNER != 0 && flags & Self::TOSTRING_USE_INNER == 0 {
            return Err(throw(
                req,
                "InvalidArgumentException",
                "Unsetting flag TOSTRING_USE_INNER is not possible",
            ));
        }
        if flags & Self::FULL_CACHE != 0 && old & Self::FULL_CACHE == 0 {
            let fresh = PhpArray::new(req)?;
            let old = dual(req, this, |it| it.cache.replace(fresh))?;
            crate::give_up(req, old);
        }
        dual(req, this, |it| {
            it.flags = (it.flags & !CIT_PUBLIC) | (flags & CIT_PUBLIC);
        })
    }

    /// `offsetGet($key): mixed`: the cached element, the warning for one
    /// not cached.
    #[php(tentative)]
    fn offsetGet(
        req: &mut Request,
        this: This<Self>,
        key: Untyped<PhpString>,
    ) -> Result<Value, Thrown> {
        let this = checked(req, this.object())?;
        let cache = full_cache(req, this)?;
        let key = Key::lookup(key.0);
        if let Some(value) = cache.get(&key) {
            return Ok(value.deref());
        }
        let shown = match &key {
            Key::Int(n) => n.to_string(),
            Key::Str(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
        };
        php_ext::raising::warning(req, &format!("Undefined array key \"{shown}\""));
        Ok(Value::null())
    }

    /// `offsetSet($key, mixed $value): void`.
    #[php(tentative)]
    fn offsetSet(
        req: &mut Request,
        this: This<Self>,
        key: Untyped<PhpString>,
        value: Value,
    ) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        full_cache(req, this)?;
        let key = Owner::take(req, Key::lookup(key.0));
        let value = Owner::take(req, value.deref());
        cache_write(req, this, |req, cache| cache.set(req, key, value))
    }

    /// `offsetUnset($key): void`.
    #[php(tentative)]
    fn offsetUnset(
        req: &mut Request,
        this: This<Self>,
        key: Untyped<PhpString>,
    ) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        full_cache(req, this)?;
        cache_write(req, this, |req, cache| {
            cache.unset(req, &Key::lookup(key.0)).map(drop)
        })
    }

    /// `offsetExists($key): bool`.
    #[php(tentative)]
    fn offsetExists(
        req: &mut Request,
        this: This<Self>,
        key: Untyped<PhpString>,
    ) -> Result<bool, Thrown> {
        let this = checked(req, this.object())?;
        let cache = full_cache(req, this)?;
        Ok(cache.contains(&Key::lookup(key.0)))
    }

    /// `getCache(): array`.
    #[php(tentative)]
    fn getCache(req: &mut Request, this: This<Self>) -> Result<PhpArray, Thrown> {
        let this = checked(req, this.object())?;
        full_cache(req, this)
    }

    /// `count(): int`, the cached elements.
    #[php(tentative)]
    fn count(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let this = checked(req, this.object())?;
        let cache = full_cache(req, this)?;
        Ok(i64::try_from(cache.len()).unwrap_or(i64::MAX))
    }
}

/// `class RecursiveCachingIterator extends CachingIterator implements
/// RecursiveIterator`: each element's children made as it is fetched.
///
/// ```php
/// $it = new RecursiveIteratorIterator(new RecursiveCachingIterator(new RecursiveArrayIterator([1, [2, 3]]), 0), RecursiveIteratorIterator::SELF_FIRST);
/// foreach ($it as $k => $v) echo $k, is_array($v) ? 'A' : $v, ";";
/// $c = new RecursiveCachingIterator(new RecursiveArrayIterator([[1]]));
/// $c->rewind(); var_dump($c->hasChildren(), get_class($c->getChildren()));
/// ```
/// ```expect
/// 01;1A;02;13;
/// Warning: Array to string conversion in %s on line 5
/// bool(true)
/// string(24) "RecursiveCachingIterator"
/// ```
#[php_class(extends(CachingIterator), implements(RecursiveIterator))]
pub(crate) struct RecursiveCachingIterator;

#[php_class]
impl RecursiveCachingIterator {
    /// `RecursiveCachingIterator::__construct(Iterator $iterator, int $flags = CALL_TOSTRING)`.
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Raw<Instance<Iterator>>,
        #[php(default = RecursiveCachingIterator::CALL_TOSTRING)] flags: Raw<i64>,
    ) -> Result<(), Thrown> {
        construct_caching(
            req,
            this.object(),
            "RecursiveCachingIterator",
            |req| {
                // `spl_dual_it_construct`'s `"O|l"` over
                // `spl_ce_RecursiveIterator`, where the stub declares
                // `Iterator $iterator`.
                let ty = <Instance<RecursiveIterator> as FromArgument>::declared();
                let ty = ty.unwrap_or(Type::Object);
                let held = coerce_param(req, &iterator.value(), &ty, 1)?;
                Ok(Instance::<RecursiveIterator>::from_value(req, held)?.object())
            },
            flags,
            Kind::RecursiveCaching,
        )
    }

    /// `hasChildren(): bool`: whether children were made for the element.
    #[php(tentative)]
    fn hasChildren(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = checked(req, this.object())?;
        dual(req, this, |it| it.children.is_some())
    }

    /// `getChildren(): ?RecursiveCachingIterator`.
    #[php(tentative)]
    fn getChildren(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Instance<RecursiveCachingIterator>>, Thrown> {
        let this = checked(req, this.object())?;
        let children = dual(req, this, |it| it.children.as_deref().copied())?;
        Ok(children.map(Instance::of))
    }
}

/// `class NoRewindIterator extends IteratorIterator`: the inner
/// iterator's own methods, `rewind()` left out.
///
/// ```php
/// $inner = new ArrayIterator([1, 2, 3]);
/// $it = new NoRewindIterator($inner);
/// foreach ($it as $k => $v) { echo $v, $it->key(); break; }
/// $inner->next();
/// foreach ($it as $v) echo $v;
/// ```
/// ```expect
/// 1023
/// ```
#[php_class(extends(IteratorIterator))]
pub(crate) struct NoRewindIterator;

#[php_class]
impl NoRewindIterator {
    /// `NoRewindIterator::__construct(Iterator $iterator)`.
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Raw<Instance<Iterator>>,
    ) -> Result<(), Thrown> {
        construct_inner(
            req,
            this.object(),
            "NoRewindIterator",
            iterator,
            Kind::NoRewind,
        )
    }

    /// `NoRewindIterator::rewind(): void`: nothing.
    #[php(tentative)]
    fn rewind(this: This<Self>) {
        let _ = this;
    }

    /// `NoRewindIterator::valid(): bool`.
    #[php(tentative)]
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = checked(req, this.object())?;
        dual_valid(req, this)
    }

    /// `NoRewindIterator::key(): mixed`, the inner iterator's
    /// `get_current_key`.
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<Owner<Value>, Thrown> {
        match no_rewind_iter(req, this.object())? {
            Some(iter) => iter.key(req),
            None => Ok(Owner::null()),
        }
    }

    /// `NoRewindIterator::current(): mixed`, the inner iterator's
    /// `get_current_data`, which a user's `Iterator` keeps until the walk
    /// moves, so `current()` is served once per element.
    ///
    /// ```php
    /// class It extends ArrayIterator { function current(): mixed { echo "current;"; return parent::current(); } }
    /// $n = new NoRewindIterator(new It([1, 2]));
    /// echo $n->current(), $n->current(), "\n";
    /// $n->next(); echo $n->current(), "\n";
    /// ```
    /// ```expect
    /// current;11
    /// current;2
    /// ```
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<Owner<Value>, Thrown> {
        match no_rewind_iter(req, this.object())? {
            Some(iter) => {
                let data = iter.value(req)?;
                Ok(crate::dereferenced(req, data))
            }
            None => Ok(Owner::null()),
        }
    }

    /// `NoRewindIterator::next(): void`, the inner iterator's
    /// `move_forward`.
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        match no_rewind_iter(req, this.object())? {
            Some(iter) => iter.next(req),
            None => Ok(()),
        }
    }
}

/// `class AppendIterator extends IteratorIterator`: one iterator after
/// another, the list of them an `ArrayIterator`.
///
/// ```php
/// $it = new AppendIterator;
/// $it->append(new ArrayIterator(['a' => 1]));
/// $it->append(new ArrayIterator([]));
/// $it->append(new ArrayIterator(['b' => 2, 'c' => 3]));
/// foreach ($it as $k => $v) echo "$k=$v@", $it->getIteratorIndex(), ";";
/// echo "\n", count($it->getArrayIterator()), "\n";
/// $it->rewind(); var_dump($it->valid(), $it->current());
/// ```
/// ```expect
/// a=1@0;b=2@2;c=3@2;
/// 3
/// bool(true)
/// int(1)
/// ```
#[php_class(extends(IteratorIterator))]
pub(crate) struct AppendIterator;

#[php_class]
impl AppendIterator {
    /// `AppendIterator::__construct()`: an `ArrayIterator` of the iterators.
    #[php(arity = manual)]
    fn __construct(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = this.object();
        unconstructed(req, this, "AppendIterator")?;
        arity(req, 0, Some(0))?;
        let Some(class) = lookup_class(req, b"ArrayIterator")? else {
            return Err(php_ext::raising::fatal(
                req,
                "InternalError: AppendIterator::__construct served with ArrayIterator unlinked",
            ));
        };
        let list = instantiate(req, class, &[])?;
        let old = dual(req, this, |it| {
            it.kind = Kind::Append;
            it.appended.replace(list)
        })?;
        crate::give_up(req, old);
        Ok(())
    }

    /// `append(Iterator $iterator): void`.
    #[php(tentative)]
    fn append(
        req: &mut Request,
        this: This<Self>,
        iterator: Instance<Iterator>,
    ) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        let iterator = iterator.object();
        let list = appended(req, this)?;
        let list_valid = crate::valid(req, list)?;
        let inner_valid = dual_valid(req, this)?;
        let answered = call_method(req, &list, b"append", &[iterator.as_value()])?;
        answered.give_up(req);
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
        Ok(())
    }

    /// `AppendIterator::rewind(): void`.
    #[php(tentative)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        let list = appended(req, this)?;
        crate::effect(req, list, b"rewind")?;
        if append_next_iterator(req, this)? {
            append_fetch(req, this)?;
        }
        Ok(())
    }

    #[php(tentative, implementation = IteratorIterator::valid)]
    fn valid(this: This<Self>) -> bool;

    /// `AppendIterator::current(): mixed`: fetched again.
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let this = checked(req, this.object())?;
        dual_fetch(req, this, true)?;
        let data = dual(req, this, |it| it.data.as_deref().copied())?;
        Ok(data.map_or_else(Value::null, |d| d.deref()))
    }

    /// `AppendIterator::next(): void`.
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        if dual_valid(req, this)? {
            dual_next(req, this, true)?;
        }
        append_fetch(req, this)
    }

    /// `getIteratorIndex(): ?int`, the list's key.
    #[php(tentative)]
    fn getIteratorIndex(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Raw<Option<i64>>>, Thrown> {
        let this = checked(req, this.object())?;
        let list = appended(req, this)?;
        let key = crate::method(req, list, b"key")?;
        Ok(crate::answered(key))
    }

    /// `getArrayIterator(): ArrayIterator`.
    #[php(tentative)]
    fn getArrayIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Instance<ArrayIterator>, Thrown> {
        let this = checked(req, this.object())?;
        Ok(Instance::of(appended(req, this)?))
    }
}

/// `class InfiniteIterator extends IteratorIterator`: past the end,
/// rewound.
///
/// ```php
/// $it = new InfiniteIterator(new ArrayIterator([1, 2]));
/// $n = 0; foreach ($it as $v) { echo $v; if (++$n == 5) break; }
/// ```
/// ```expect
/// 12121
/// ```
#[php_class(extends(IteratorIterator))]
pub(crate) struct InfiniteIterator;

#[php_class]
impl InfiniteIterator {
    /// `InfiniteIterator::__construct(Iterator $iterator)`.
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Raw<Instance<Iterator>>,
    ) -> Result<(), Thrown> {
        construct_inner(
            req,
            this.object(),
            "InfiniteIterator",
            iterator,
            Kind::Infinite,
        )
    }

    /// `InfiniteIterator::next(): void`; the witness is the constructor's.
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = checked(req, this.object())?;
        dual_next(req, this, true)?;
        if dual_valid(req, this)? {
            dual_fetch(req, this, false)?;
        } else {
            dual_rewind(req, this)?;
            if dual_valid(req, this)? {
                dual_fetch(req, this, false)?;
            }
        }
        Ok(())
    }
}

// ---- `RegexIterator` and `RecursiveRegexIterator`, over ext/pcre's pattern

/// `class RegexIterator extends FilterIterator`: the elements, or keys,
/// a pattern matches, each as `getMode()` makes of it.
#[php_class(extends(FilterIterator))]
pub(crate) struct RegexIterator;

#[php_class]
impl RegexIterator {
    const USE_KEY: i64 = 1;
    const INVERT_MATCH: i64 = 2;
    const MATCH: i64 = Mode::Match as i64;
    const GET_MATCH: i64 = Mode::GetMatch as i64;
    const ALL_MATCHES: i64 = Mode::AllMatches as i64;
    const SPLIT: i64 = Mode::Split as i64;
    const REPLACE: i64 = Mode::Replace as i64;

    #[php(property)]
    const replacement: Option<PhpString> = None;

    /// `RegexIterator::__construct(Iterator $iterator, string $pattern, int $mode = RegexIterator::MATCH, int $flags = 0, int $pregFlags = 0)`:
    /// the pattern out of ext/pcre's cache, its warning thrown as
    /// `InvalidArgumentException`.
    ///
    /// ```php
    /// $it = new RegexIterator(new ArrayIterator(['apple', 'bob', 'avocado', 7, 'a' => [1]]), '/^a/');
    /// foreach ($it as $k => $v) echo "$k=$v;";
    /// echo "\n";
    /// try { new RegexIterator(new ArrayIterator([]), '/(/'); } catch (InvalidArgumentException $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// try { new RegexIterator(new ArrayIterator([]), 'abc'); } catch (InvalidArgumentException $e) { echo $e->getMessage(), "\n"; }
    /// try { new RegexIterator(new ArrayIterator([]), '/a/', 5); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// $twice = new RegexIterator(new ArrayIterator([]), '/a/');
    /// try { $twice->__construct(new ArrayIterator([]), '/a/'); } catch (BadMethodCallException $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($twice->replacement);
    /// ```
    /// ```expect
    /// 0=apple;2=avocado;
    /// InvalidArgumentException: RegexIterator::__construct(): Compilation failed: missing closing parenthesis at offset 1
    /// RegexIterator::__construct(): Delimiter must not be alphanumeric, backslash, or NUL byte
    /// RegexIterator::__construct(): Argument #3 ($mode) must be RegexIterator::MATCH, RegexIterator::GET_MATCH, RegexIterator::ALL_MATCHES, RegexIterator::SPLIT, or RegexIterator::REPLACE
    /// RegexIterator::getIterator() must be called exactly once per instance
    /// NULL
    /// ```
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Raw<Instance<Iterator>>,
        pattern: Raw<PhpString>,
        #[php(default = RegexIterator::MATCH)] mode: Raw<i64>,
        #[php(default = 0)] flags: Raw<i64>,
        #[php(default = 0)] pregFlags: Raw<i64>,
    ) -> Result<(), Thrown> {
        construct_regex(
            req,
            this.object(),
            "RegexIterator",
            iterator,
            (pattern, mode, flags, pregFlags),
            Kind::Regex,
        )
    }

    /// `RegexIterator::accept(): bool`: `(string)current()`, or the key
    /// under `USE_KEY`, matched against the pattern as `getMode()` says, the
    /// answer turned over under `INVERT_MATCH`; `GET_MATCH`, `ALL_MATCHES`
    /// and `SPLIT` put what they found in place of the element, and
    /// `REPLACE` puts the replaced subject in place of the element or the
    /// key.
    ///
    /// ```php
    /// $in = new ArrayIterator(['k1' => 'a-1', 'k' => 'b', 'k3' => 'c-3-4', 'x' => [1]]);
    /// $show = function ($it) { foreach ($it as $k => $v) echo $k, '=', is_array($v) ? str_replace(["\n", ' '], '', print_r($v, true)) : $v, ';'; echo "\n"; };
    /// $show(new RegexIterator($in, '/\d/'));
    /// $show(new RegexIterator($in, '/\d/', RegexIterator::MATCH, RegexIterator::INVERT_MATCH));
    /// $show(new RegexIterator($in, '/\d/', RegexIterator::MATCH, RegexIterator::USE_KEY));
    /// $show(new RegexIterator($in, '/(\w)-(\d)/', RegexIterator::GET_MATCH));
    /// $show(new RegexIterator($in, '/\d/', RegexIterator::ALL_MATCHES));
    /// $show(new RegexIterator($in, '/-/', RegexIterator::SPLIT));
    /// $show(new RegexIterator($in, '/-/', RegexIterator::SPLIT, 0, PREG_SPLIT_OFFSET_CAPTURE));
    /// $r = new RegexIterator($in, '/(\w)-/', RegexIterator::REPLACE);
    /// $show($r);
    /// $r->replacement = '<$1>';
    /// $show($r);
    /// $k = new RegexIterator($in, '/k/', RegexIterator::REPLACE, RegexIterator::USE_KEY);
    /// $k->replacement = 'K';
    /// $show($k);
    /// ```
    /// ```expect
    /// k1=a-1;k3=c-3-4;
    /// k=b;
    /// k1=a-1;k3=c-3-4;
    /// k1=Array([0]=>a-1[1]=>a[2]=>1);k3=Array([0]=>c-3[1]=>c[2]=>3);
    /// k1=Array([0]=>Array([0]=>1));k3=Array([0]=>Array([0]=>3[1]=>4));
    /// k1=Array([0]=>a[1]=>1);k3=Array([0]=>c[1]=>3[2]=>4);
    /// k1=Array([0]=>Array([0]=>a[1]=>0)[1]=>Array([0]=>1[1]=>2));k3=Array([0]=>Array([0]=>c[1]=>0)[1]=>Array([0]=>3[1]=>2)[2]=>Array([0]=>4[1]=>4));
    /// k1=1;k3=4;
    /// k1=<a>1;k3=<c><3>4;
    /// K1=a-1;K=b;K3=c-3-4;
    /// ```
    #[php(tentative)]
    fn accept(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        regex_accept(req, this.object())
    }

    /// `RegexIterator::getMode(): int`.
    ///
    /// ```php
    /// var_dump((new RegexIterator(new ArrayIterator([]), '/a/'))->getMode());
    /// var_dump((new RegexIterator(new ArrayIterator([]), '/a/', RegexIterator::REPLACE))->getMode());
    /// ```
    /// ```expect
    /// int(0)
    /// int(4)
    /// ```
    #[php(tentative)]
    fn getMode(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        Ok(regex_of(req, this.object())?.mode.number())
    }

    /// `RegexIterator::setMode(int $mode): void`, `$mode` checked before
    /// the receiver, as php checks it.
    ///
    /// ```php
    /// $it = new RegexIterator(new ArrayIterator(['a-b', 'c']), '/-/');
    /// $it->setMode(RegexIterator::SPLIT);
    /// var_dump($it->getMode(), iterator_to_array($it));
    /// try { $it->setMode(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($it->getMode());
    /// ```
    /// ```expect
    /// int(3)
    /// array(1) {
    ///   [0]=>
    ///   array(2) {
    ///     [0]=>
    ///     string(1) "a"
    ///     [1]=>
    ///     string(1) "b"
    ///   }
    /// }
    /// RegexIterator::setMode(): Argument #1 ($mode) must be RegexIterator::MATCH, RegexIterator::GET_MATCH, RegexIterator::ALL_MATCHES, RegexIterator::SPLIT, or RegexIterator::REPLACE
    /// int(3)
    /// ```
    #[php(tentative)]
    fn setMode(req: &mut Request, this: This<Self>, mode: i64) -> Result<(), Thrown> {
        let Some(mode) = Mode::named(mode) else {
            return Err(arg_value_error(req, 1, MODE_MESSAGE));
        };
        regex_set(req, this.object(), |regex| regex.mode = mode)
    }

    /// `RegexIterator::getFlags(): int`.
    ///
    /// ```php
    /// var_dump((new RegexIterator(new ArrayIterator([]), '/a/'))->getFlags());
    /// var_dump((new RegexIterator(new ArrayIterator([]), '/a/', RegexIterator::MATCH, RegexIterator::USE_KEY | RegexIterator::INVERT_MATCH))->getFlags());
    /// ```
    /// ```expect
    /// int(0)
    /// int(3)
    /// ```
    #[php(tentative)]
    fn getFlags(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        Ok(regex_of(req, this.object())?.flags.0)
    }

    /// `RegexIterator::setFlags(int $flags): void`, any bits.
    ///
    /// ```php
    /// $it = new RegexIterator(new ArrayIterator(['a' => 'x', 'y' => 'a']), '/a/');
    /// echo implode(',', array_keys(iterator_to_array($it))), "\n";
    /// $it->setFlags(RegexIterator::USE_KEY);
    /// echo implode(',', array_keys(iterator_to_array($it))), "\n";
    /// $it->setFlags(RegexIterator::USE_KEY | RegexIterator::INVERT_MATCH | 64);
    /// echo implode(',', array_keys(iterator_to_array($it))), "\n";
    /// var_dump($it->getFlags());
    /// ```
    /// ```expect
    /// y
    /// a
    /// y
    /// int(67)
    /// ```
    #[php(tentative)]
    fn setFlags(req: &mut Request, this: This<Self>, flags: i64) -> Result<(), Thrown> {
        regex_set(req, this.object(), |regex| regex.flags = RegexFlags(flags))
    }

    /// `RegexIterator::getRegex(): string`, the pattern as it was given.
    ///
    /// ```php
    /// $it = new RegexIterator(new ArrayIterator([]), '/^x(\d)$/i');
    /// var_dump($it->getRegex());
    /// ```
    /// ```expect
    /// string(10) "/^x(\d)$/i"
    /// ```
    #[php(tentative)]
    fn getRegex(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let regex = regex_of(req, this.object())?;
        PhpString::new(req, &regex.source)
    }

    /// `RegexIterator::getPregFlags(): int`.
    ///
    /// ```php
    /// var_dump((new RegexIterator(new ArrayIterator([]), '/a/'))->getPregFlags());
    /// var_dump((new RegexIterator(new ArrayIterator([]), '/a/', RegexIterator::SPLIT, 0, PREG_SPLIT_NO_EMPTY))->getPregFlags());
    /// ```
    /// ```expect
    /// int(0)
    /// int(1)
    /// ```
    #[php(tentative)]
    fn getPregFlags(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        Ok(regex_of(req, this.object())?.preg_flags)
    }

    /// `RegexIterator::setPregFlags(int $pregFlags): void`: the `PREG_*`
    /// flags the `php_pcre_*_impl` that `getMode()` names is served with.
    ///
    /// ```php
    /// $it = new RegexIterator(new ArrayIterator(['ab']), '/(b)/', RegexIterator::GET_MATCH);
    /// $it->setPregFlags(PREG_OFFSET_CAPTURE);
    /// var_dump($it->getPregFlags());
    /// foreach ($it as $v) echo json_encode($v), "\n";
    /// ```
    /// ```expect
    /// int(256)
    /// [["b",1],["b",1]]
    /// ```
    #[php(tentative)]
    fn setPregFlags(req: &mut Request, this: This<Self>, pregFlags: i64) -> Result<(), Thrown> {
        regex_set(req, this.object(), |regex| regex.preg_flags = pregFlags)
    }
}

/// `class RecursiveRegexIterator extends RegexIterator implements
/// RecursiveIterator`.
#[php_class(extends(RegexIterator), implements(RecursiveIterator))]
pub(crate) struct RecursiveRegexIterator;

#[php_class]
impl RecursiveRegexIterator {
    /// `RecursiveRegexIterator::__construct(RecursiveIterator $iterator, string $pattern, int $mode = RecursiveRegexIterator::MATCH, int $flags = 0, int $pregFlags = 0)`.
    ///
    /// ```php
    /// $it = new RecursiveIteratorIterator(new RecursiveRegexIterator(new RecursiveArrayIterator(['ax', ['ay', 'b'], 'c']), '/a/'), RecursiveIteratorIterator::SELF_FIRST);
    /// foreach ($it as $k => $v) echo $it->getDepth(), ':', is_array($v) ? 'A' : $v, ';';
    /// echo "\n";
    /// try { new RecursiveRegexIterator(new ArrayIterator([]), '/a/'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// 0:ax;0:A;1:ay;
    /// RecursiveRegexIterator::__construct(): Argument #1 ($iterator) must be of type RecursiveIterator, ArrayIterator given
    /// ```
    #[php(arity = manual)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Raw<Instance<RecursiveIterator>>,
        pattern: Raw<PhpString>,
        #[php(default = RecursiveRegexIterator::MATCH)] mode: Raw<i64>,
        #[php(default = 0)] flags: Raw<i64>,
        #[php(default = 0)] pregFlags: Raw<i64>,
    ) -> Result<(), Thrown> {
        construct_regex(
            req,
            this.object(),
            "RecursiveRegexIterator",
            iterator,
            (pattern, mode, flags, pregFlags),
            Kind::RecursiveRegex,
        )
    }

    /// `RecursiveRegexIterator::accept(): bool`: an array element is
    /// accepted where it is not empty, and anything else is
    /// `RegexIterator::accept`'s.
    ///
    /// ```php
    /// $it = new RecursiveRegexIterator(new RecursiveArrayIterator(['x1', [], ['y'], 'z']), '/\d/');
    /// foreach ($it as $k => $v) echo $k, is_array($v) ? '=A' : "=$v", ';';
    /// echo "\n";
    /// $it = new RecursiveRegexIterator(new RecursiveArrayIterator(['x1', ['y']]), '/\d/', RecursiveRegexIterator::MATCH, RecursiveRegexIterator::INVERT_MATCH);
    /// foreach ($it as $k => $v) echo $k, is_array($v) ? '=A' : "=$v", ';';
    /// echo "\n";
    /// ```
    /// ```expect
    /// 0=x1;2=A;
    /// 1=A;
    /// ```
    #[php(tentative)]
    fn accept(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = checked(req, this.object())?;
        match dual(req, this, |it| it.data.as_deref().copied())? {
            None => Ok(false),
            Some(data) => match data.as_array() {
                Some(array) => Ok(!array.is_empty()),
                None => regex_accept(req, this),
            },
        }
    }

    #[php(tentative, implementation = RecursiveFilterIterator::hasChildren)]
    fn hasChildren(this: This<Self>) -> bool;

    /// `RecursiveRegexIterator::getChildren(): RecursiveRegexIterator`:
    /// the inner iterator's children in a new object of the receiver's
    /// class, constructed with the receiver's pattern and its three numbers.
    ///
    /// ```php
    /// class Tagged extends RecursiveRegexIterator {}
    /// $it = new Tagged(new RecursiveArrayIterator([['a1', 'b2', 'a3']]), '/^a(\d)/', RegexIterator::GET_MATCH, 0, 0);
    /// $it->rewind();
    /// $children = $it->getChildren();
    /// var_dump(get_class($children), $children->getRegex(), $children->getMode());
    /// foreach ($children as $k => $v) echo $k, ':', implode(',', $v), ';';
    /// echo "\n";
    /// ```
    /// ```expect
    /// string(6) "Tagged"
    /// string(8) "/^a(\d)/"
    /// int(1)
    /// 0:a1,1;2:a3,3;
    /// ```
    #[php(tentative)]
    fn getChildren(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<RecursiveRegexIterator>>, Thrown> {
        let this = this.object();
        let regex = regex_of(req, this)?;
        let Some(inner) = inner(req, this)? else {
            return Err(php_ext::raising::fatal(
                req,
                "InternalError: a RecursiveRegexIterator constructed without its inner iterator",
            ));
        };
        let children = crate::method(req, inner, b"getChildren")?;
        let source = match crate::string_value(req, &regex.source) {
            Ok(source) => source,
            Err(thrown) => {
                children.give_up(req);
                return Err(thrown);
            }
        };
        let arguments = [
            *children,
            *source,
            Value::int(regex.mode.number()),
            Value::int(regex.flags.0),
            Value::int(regex.preg_flags),
        ];
        let made = instantiate(req, this.class(), &arguments);
        children.give_up(req);
        source.give_up(req);
        Ok(made?.into_instance())
    }
}

/// `class EmptyIterator implements Iterator`: no element.
///
/// ```php
/// $e = new EmptyIterator;
/// foreach ($e as $v) echo "never";
/// var_dump($e->valid(), iterator_count($e));
/// try { $e->current(); } catch (BadMethodCallException $x) { echo $x->getMessage(), "\n"; }
/// try { $e->key(); } catch (BadMethodCallException $x) { echo $x->getMessage(), "\n"; }
/// $e->next(); $e->rewind();
/// ```
/// ```expect
/// bool(false)
/// int(0)
/// Accessing the value of an EmptyIterator
/// Accessing the key of an EmptyIterator
/// ```
#[php_class(implements(Iterator))]
pub(crate) struct EmptyIterator;

#[php_class]
impl EmptyIterator {
    /// `EmptyIterator::current(): never`.
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<Never, Thrown> {
        let _ = this;
        Err(throw(
            req,
            "BadMethodCallException",
            "Accessing the value of an EmptyIterator",
        ))
    }

    #[php(tentative, implementation = EmptyIterator::rewind)]
    fn next(this: This<Self>);

    /// `EmptyIterator::key(): never`.
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<Never, Thrown> {
        let _ = this;
        Err(throw(
            req,
            "BadMethodCallException",
            "Accessing the key of an EmptyIterator",
        ))
    }

    /// `EmptyIterator::valid(): false`.
    #[php(tentative)]
    fn valid(this: This<Self>) -> False {
        let _ = this;
        False
    }

    /// `EmptyIterator::rewind(): void`, and `next()`: nothing.
    #[php(tentative)]
    fn rewind(this: This<Self>) {
        let _ = this;
    }
}

/// `class RecursiveTreeIterator extends RecursiveIteratorIterator`: each
/// element drawn as a line of an ASCII tree.
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
#[php_class(extends(RecursiveIteratorIterator))]
pub(crate) struct RecursiveTreeIterator;

#[php_class]
impl RecursiveTreeIterator {
    const BYPASS_CURRENT: i64 = 4;
    const BYPASS_KEY: i64 = 8;
    const PREFIX_LEFT: i64 = 0;
    const PREFIX_MID_HAS_NEXT: i64 = 1;
    const PREFIX_MID_LAST: i64 = 2;
    const PREFIX_END_HAS_NEXT: i64 = 3;
    const PREFIX_END_LAST: i64 = 4;
    const PREFIX_RIGHT: i64 = 5;

    /// `RecursiveTreeIterator::__construct()`, over a `RecursiveIterator`
    /// or an `IteratorAggregate`, with `BYPASS_KEY`, `CATCH_GET_CHILD` and
    /// `SELF_FIRST` by default: the iterator wrapped in a `RecursiveCachingIterator`, which is what
    /// `hasNext()` is asked of for the prefix.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        iterator: Raw<Union<Instance<RecursiveIterator>, Instance<IteratorAggregate>>>,
        #[php(default = RecursiveTreeIterator::BYPASS_KEY)] flags: Raw<i64>,
        #[php(default = CachingIterator::CATCH_GET_CHILD)] cachingIteratorFlags: Raw<i64>,
        #[php(default = RecursiveTreeIterator::SELF_FIRST)] mode: Raw<i64>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let Some(given) = iterator.value().as_object() else {
            return Err(arg_type_error(
                req,
                1,
                "be of type RecursiveIterator|IteratorAggregate",
            ));
        };
        let flags = flags.hold(req)?;
        let caching_flags = cachingIteratorFlags.hold(req)?;
        let mode = mode.hold(req)?;
        let source = if crate::is_a(req, given.class(), b"IteratorAggregate") {
            let answered = call_method(req, &given, b"getIterator", &[])?;
            if !answered
                .as_object()
                .is_some_and(|o| crate::is_a(req, o.class(), b"Traversable"))
            {
                answered.give_up(req);
                return Err(throw_exception_ex(
                    req,
                    "LogicException",
                    "getIterator() must return an object that implements Traversable",
                ));
            }
            answered
        } else {
            Owner::take(req, given.as_value())
        };
        let Some(class) = lookup_class(req, b"RecursiveCachingIterator")? else {
            source.give_up(req);
            return Err(php_ext::raising::fatal(
                req,
                "InternalError: RecursiveTreeIterator::__construct served with RecursiveCachingIterator unlinked",
            ));
        };
        let caching = instantiate(req, class, &[*source, Value::int(caching_flags)]);
        source.give_up(req);
        let caching = caching?;
        let built = rec_construct(req, this, *caching, mode, flags);
        caching.give_up(req);
        built?;
        let tree = Tree::new(req)?;
        if let Some(old) = rec(req, this, |it| it.tree.replace(tree))? {
            old.release(req);
        }
        Ok(())
    }

    /// `key(): mixed`: the key drawn with the prefix, or as it is under
    /// `BYPASS_KEY`.
    ///
    /// ```php
    /// $tree = new RecursiveTreeIterator(new RecursiveArrayIterator(['a' => [1]]), 0);
    /// foreach ($tree as $k => $v) echo $k, "\n";
    /// $b = new RecursiveTreeIterator(new RecursiveArrayIterator([[7]]), RecursiveTreeIterator::BYPASS_CURRENT);
    /// $b->rewind(); var_dump($b->current());
    /// ```
    /// ```expect
    /// \-a
    ///   \-0
    /// array(1) {
    ///   [0]=>
    ///   int(7)
    /// }
    /// ```
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<Owner<Value>, Thrown> {
        let this = rec_checked(req, this.object())?;
        let Some(level) = top(req, this)? else {
            return Ok(Owner::null());
        };
        let bypass = rec(req, this, |it| it.flags & Self::BYPASS_KEY != 0)?;
        let key = level.sub.key(req)?;
        let key = crate::dereferenced(req, key);
        if bypass {
            return Ok(key);
        }
        let text = with_string(req, &key, <[u8]>::to_vec);
        key.give_up(req);
        let text = text?;
        let mut out = tree_prefix(req, this)?;
        out.extend(text);
        out.extend(postfix(req, this)?);
        crate::string_value(req, &out)
    }

    /// `current(): mixed`: the entry drawn with the prefix and postfix, or
    /// the element under `BYPASS_CURRENT`.
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<Owner<Value>, Thrown> {
        let this = rec_checked(req, this.object())?;
        if rec(req, this, |it| it.flags & Self::BYPASS_CURRENT != 0)? {
            // The element the inner iterator answered is its one owner.
            return rec_current(req, this);
        }
        let Some(entry) = tree_entry(req, this)? else {
            return Ok(Owner::null());
        };
        let mut out = tree_prefix(req, this)?;
        out.extend(entry);
        out.extend(postfix(req, this)?);
        crate::string_value(req, &out)
    }

    /// `getPrefix(): string`; the witness is the constructor's.
    #[php(tentative)]
    fn getPrefix(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let this = rec_checked(req, this.object())?;
        let prefix = tree_prefix(req, this)?;
        PhpString::new(req, &prefix)
    }

    /// `setPostfix(string $postfix): void`.
    #[php(tentative)]
    fn setPostfix(req: &mut Request, this: This<Self>, postfix: PhpString) -> Result<(), Thrown> {
        let postfix = Owner::take(req, postfix);
        let old = rec(req, this.object(), |it| match it.tree.as_mut() {
            Some(tree) => std::mem::replace(&mut tree.postfix, postfix),
            None => postfix,
        })?;
        old.give_up(req);
        Ok(())
    }

    /// `setPrefixPart(int $part, string $value): void`.
    #[php(tentative)]
    fn setPrefixPart(
        req: &mut Request,
        this: This<Self>,
        part: i64,
        value: PhpString,
    ) -> Result<(), Thrown> {
        let Some(part) = usize::try_from(part)
            .ok()
            .filter(|p| i64::try_from(*p).is_ok_and(|p| p <= Self::PREFIX_RIGHT))
        else {
            return Err(arg_value_error(
                req,
                1,
                "be a RecursiveTreeIterator::PREFIX_* constant",
            ));
        };
        let value = Owner::take(req, value);
        let old = rec(req, this.object(), |it| {
            match it.tree.as_mut().and_then(|tree| tree.prefix.get_mut(part)) {
                Some(slot) => std::mem::replace(slot, value),
                None => value,
            }
        })?;
        old.give_up(req);
        Ok(())
    }

    /// `getEntry(): string`.
    #[php(tentative)]
    fn getEntry(req: &mut Request, this: This<Self>) -> Result<Owner<Raw<PhpString>>, Thrown> {
        let this = rec_checked(req, this.object())?;
        let entry = match tree_entry(req, this)? {
            Some(entry) => crate::string_value(req, &entry)?,
            None => Owner::null(),
        };
        Ok(crate::answered(entry))
    }

    /// `getPostfix(): string`.
    #[php(tentative)]
    fn getPostfix(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let this = rec_checked(req, this.object())?;
        let postfix = postfix(req, this)?;
        PhpString::new(req, &postfix)
    }
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

fn inner(req: &mut Request, this: PhpObject) -> Result<Option<PhpObject>, Thrown> {
    dual(req, this, |it| it.inner.as_deref().copied())
}

/// `spl_dual_it_free`: the inner iterator's kept `current()` given up,
/// the fetched element and key, and a caching iterator's string and
/// children.
fn dual_free(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    if let Some(iter) = inner_iter(req, this)? {
        iter.invalidate(req);
    }
    let (values, children) = dual(req, this, |it| {
        let children = if matches!(it.kind, Kind::Caching | Kind::RecursiveCaching) {
            it.children.take()
        } else {
            None
        };
        ([it.data.take(), it.key.take(), it.string.take()], children)
    })?;
    for value in values.into_iter().flatten() {
        value.give_up(req);
    }
    crate::give_up(req, children);
    Ok(())
}

/// The inner object's iterator, `intern->inner.iterator`.
fn inner_iter(req: &mut Request, this: PhpObject) -> Result<Option<Iter>, Thrown> {
    dual(req, this, |it| it.iter.as_deref().copied())
}

/// `intern->inner.iterator = ce->get_iterator(ce, zobject, 0)`: the
/// iterator of the inner object just set, which a finished generator
/// refuses.
fn inner_iterate(req: &mut Request, this: PhpObject, inner: PhpObject) -> Result<(), Thrown> {
    let iter = iterate(req, &inner)?;
    if let Some(old) = dual(req, this, |it| it.iter.replace(iter))? {
        old.give_up(req);
    }
    Ok(())
}

/// `spl_dual_it_rewind`.
fn dual_rewind(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    dual_free(req, this)?;
    dual(req, this, |it| it.pos = 0)?;
    if let Some(iter) = inner_iter(req, this)? {
        iter.rewind(req)?;
    }
    Ok(())
}

/// `spl_dual_it_valid`.
fn dual_valid(req: &mut Request, this: PhpObject) -> Result<bool, Thrown> {
    match inner_iter(req, this)? {
        Some(iter) => iter.valid(req),
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
    let Some(iter) = inner_iter(req, this)? else {
        return Ok(false);
    };
    let data = iter.value(req)?;
    let data = crate::dereferenced(req, data);
    let old = dual(req, this, |it| it.data.replace(data))?;
    crate::give_up(req, old);
    let key = iter.key(req)?;
    let key = crate::dereferenced(req, key);
    let old = dual(req, this, |it| it.key.replace(key))?;
    crate::give_up(req, old);
    Ok(true)
}

/// `spl_dual_it_next`.
fn dual_next(req: &mut Request, this: PhpObject, free: bool) -> Result<(), Thrown> {
    if free {
        dual_free(req, this)?;
    }
    let Some(iter) = inner_iter(req, this)? else {
        return Err(throw_error(
            req,
            "Error",
            "The inner constructor wasn't initialized with an iterator instance",
        ));
    };
    iter.next(req)?;
    dual(req, this, |it| it.pos += 1)?;
    Ok(())
}

fn dual_set(
    req: &mut Request,
    this: PhpObject,
    kind: Kind,
    inner: PhpObject,
) -> Result<(), Thrown> {
    let held = Owner::take(req, inner);
    let old = dual(req, this, |it| {
        it.kind = kind;
        it.inner.replace(held)
    })?;
    crate::give_up(req, old);
    inner_iterate(req, this, inner)
}

/// `SPL_FETCH_AND_CHECK_DUAL_IT`: the receiver, its constructor served.
fn checked(req: &mut Request, this: PhpObject) -> Result<PhpObject, Thrown> {
    if dual(req, this, |it| it.kind)? == Kind::Unknown {
        return Err(throw_error(
            req,
            "Error",
            "The object is in an invalid state as the parent constructor was not called",
        ));
    }
    Ok(this)
}

/// `spl_dual_it_construct`'s first check, before php's ZPP: a second
/// constructor call is `BadMethodCallException`, named for `base`.
fn unconstructed(req: &mut Request, this: PhpObject, base: &str) -> Result<(), Thrown> {
    if dual(req, this, |it| it.kind)? != Kind::Unknown {
        return Err(throw_exception_ex(
            req,
            "BadMethodCallException",
            &format!("{base}::getIterator() must be called exactly once per instance"),
        ));
    }
    Ok(())
}

/// `spl_dual_it_construct` for a class that takes its inner iterator and
/// nothing else, `O`.
fn construct_inner<C: Class>(
    req: &mut Request,
    this: PhpObject,
    base: &str,
    iterator: Raw<Instance<C>>,
    kind: Kind,
) -> Result<(), Thrown> {
    unconstructed(req, this, base)?;
    arity(req, 1, Some(1))?;
    let inner = iterator.hold(req)?.object();
    dual_set(req, this, kind, inner)
}

/// `spl_dual_it_construct`'s `DIT_CallbackFilterIterator` arm, `Of`: the
/// callable kept with the owners `zend_fcc_addref` takes, so a
/// `Class::method` string resolved with `$this` in scope keeps the object
/// it was resolved to, as php's does:
///
/// ```php
/// class A { public $n = 'kept'; function __destruct(){ echo "dtor\n"; } function f($x){ echo $this->n, "\n"; return true; }
///   function make(){ return new CallbackFilterIterator(new ArrayIterator([1]), 'A::f'); } }
/// $it = (new A)->make();
/// echo "made\n";
/// foreach ($it as $v) echo $v, "\n";
/// unset($it);
/// echo "end\n";
/// ```
/// ```expect
/// made
/// kept
/// 1
/// dtor
/// end
/// ```
fn construct_callback<C: Class>(
    req: &mut Request,
    this: PhpObject,
    base: &str,
    iterator: Raw<Instance<C>>,
    callback: Raw<Callable>,
    kind: Kind,
) -> Result<(), Thrown> {
    unconstructed(req, this, base)?;
    arity(req, 2, Some(2))?;
    let inner = iterator.hold(req)?.object();
    let callback = callback.hold(req)?;
    dual_set(req, this, kind, inner)?;
    let callback = Owner::take(req, callback);
    let old = dual(req, this, |it| it.callback.replace(callback))?;
    crate::give_up(req, old);
    Ok(())
}

/// `spl_dual_it_construct`'s `DIT_CachingIterator` arm, `O|l`, the flags
/// held to at most one way of making the string; `inner` holds the first argument to the class
/// the kind reads, which `RecursiveCachingIterator` declares wider than it
/// holds.
fn construct_caching(
    req: &mut Request,
    this: PhpObject,
    base: &str,
    inner: impl FnOnce(&mut Request) -> Result<PhpObject, Thrown>,
    flags: Raw<i64>,
    kind: Kind,
) -> Result<(), Thrown> {
    unconstructed(req, this, base)?;
    arity(req, 1, Some(2))?;
    let inner = inner(req)?;
    let flags = flags.hold(req)?;
    if !caching_flags_ok(flags) {
        return Err(arg_value_error(req, 2, CIT_FLAGS_MESSAGE));
    }
    let cache = PhpArray::new(req)?;
    if let Err(thrown) = dual_set(req, this, kind, inner) {
        cache.give_up(req);
        return Err(thrown);
    }
    let old = dual(req, this, |it| {
        it.flags |= flags & CIT_PUBLIC;
        it.cache.replace(cache)
    })?;
    crate::give_up(req, old);
    Ok(())
}

/// `spl_dual_it_construct`'s `DIT_RegexIterator` arm, `OS|lll`: the
/// pattern out of ext/pcre's cache, its warning thrown as
/// `InvalidArgumentException`.
fn construct_regex<C: Class>(
    req: &mut Request,
    this: PhpObject,
    base: &str,
    iterator: Raw<Instance<C>>,
    (pattern, mode, flags, preg_flags): (Raw<PhpString>, Raw<i64>, Raw<i64>, Raw<i64>),
    kind: Kind,
) -> Result<(), Thrown> {
    unconstructed(req, this, base)?;
    arity(req, 2, Some(5))?;
    let inner = iterator.hold(req)?.object();
    let source = pattern.hold(req)?;
    let mode = mode.hold(req)?;
    let flags = flags.hold(req)?;
    let preg_flags = preg_flags.hold(req)?;
    let Some(mode) = Mode::named(mode) else {
        return Err(arg_value_error(req, 3, MODE_MESSAGE));
    };
    let compiled = throwing_warnings(req, "InvalidArgumentException", |req| {
        Ok(Regex::compile(req, source.as_bytes()))
    })?;
    let Some(pattern) = compiled else {
        return Ok(());
    };
    dual_set(req, this, kind, inner)?;
    dual(req, this, |it| {
        it.regex = Some(RegexIt {
            pattern,
            source: source.as_bytes().into(),
            mode,
            flags: RegexFlags(flags),
            preg_flags,
        });
    })
}

/// What the table refuses: cloning.
const DUAL_REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for DualHandlers {
    fn refusals(&self) -> Refusals {
        DUAL_REFUSALS
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let held = object.native_mut::<DualIt>(req).map(|it| {
            (
                [it.data.take(), it.key.take(), it.string.take()],
                [it.inner.take(), it.children.take(), it.appended.take()],
                it.cache.take(),
                it.callback.take(),
                it.iter.take(),
            )
        });
        if let Some((values, objects, cache, callback, iter)) = held {
            for value in values.into_iter().flatten() {
                value.give_up(req);
            }
            crate::give_up(req, iter);
            for object in objects.into_iter().flatten() {
                object.give_up(req);
            }
            crate::give_up(req, cache);
            crate::give_up(req, callback);
        }
    }

    /// `spl_dual_it_get_gc`: what the iterator holds, the callback of a
    /// `CallbackFilterIterator` among it (`zend_get_gc_buffer_add_fcc`),
    /// so a closure over the object that holds the iterator is a cycle
    /// the collector frees:
    ///
    /// ```php
    /// class A { public $it; function __construct() { $this->it = new CallbackFilterIterator(new ArrayIterator([1]), fn() => true); } function __destruct() { echo "freed\n"; } }
    /// $a = new A; $a = null;
    /// gc_collect_cycles();
    /// echo "done\n";
    /// ```
    /// ```expect
    /// freed
    /// done
    /// ```
    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(it) = object.native::<DualIt>(req) {
            for value in [&it.data, &it.key, &it.string].into_iter().flatten() {
                visit(**value);
            }
            if let Some(callback) = &it.callback {
                callback.visit(visit);
            }
            for object in [&it.inner, &it.children, &it.appended]
                .into_iter()
                .flatten()
            {
                visit(object.as_value());
            }
            if let Some(iter) = &it.iter {
                visit(iter.as_value());
            }
            if let Some(cache) = &it.cache {
                visit(Value::array(**cache));
            }
        }
        GcTable::Properties
    }

    /// `spl_dual_it_get_method`: the inner iterator.
    ///
    /// ```php
    /// $it = new LimitIterator(new ArrayIterator([3, 1, 2]), 0, 2);
    /// var_dump($it->count(), $it->getArrayCopy());
    /// ```
    /// ```expect
    /// int(3)
    /// array(3) {
    ///   [0]=>
    ///   int(3)
    ///   [1]=>
    ///   int(1)
    ///   [2]=>
    ///   int(2)
    /// }
    /// ```
    fn method_receiver(&self, req: &mut Request, object: PhpObject) -> Option<PhpObject> {
        object
            .native::<DualIt>(req)
            .and_then(|it| it.inner.as_deref().copied())
    }
}

/// `spl_filter_it_fetch`: elements fetched until `accept()` answers
/// something true.
fn filter_fetch(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    while dual_fetch(req, this, true)? {
        let answered = call_method(req, &this, b"accept", &[])?;
        let accepted = truthy(req, &answered);
        answered.give_up(req);
        let accepted = accepted?;
        if accepted {
            return Ok(());
        }
        if let Some(iter) = inner_iter(req, this)? {
            iter.next(req)?;
        }
    }
    dual_free(req, this)
}

/// `spl_limit_it_seek`.
fn limit_seek_to(req: &mut Request, this: PhpObject, pos: i64) -> Result<(), Thrown> {
    dual_free(req, this)?;
    let (offset, count, current) = dual(req, this, |it| (it.offset, it.count, it.pos))?;
    if pos < offset {
        return Err(throw_exception_ex(
            req,
            "OutOfBoundsException",
            &format!("Cannot seek to {pos} which is below the offset {offset}"),
        ));
    }
    if count != -1 && pos - offset >= count {
        return Err(throw_exception_ex(
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
        answered.give_up(req);
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

/// `spl_cit_check_flags`: at most one of the string flags.
fn caching_flags_ok(flags: i64) -> bool {
    [
        CachingIterator::CALL_TOSTRING,
        CachingIterator::TOSTRING_USE_KEY,
        CachingIterator::TOSTRING_USE_CURRENT,
        CachingIterator::TOSTRING_USE_INNER,
    ]
    .iter()
    .filter(|f| flags & **f != 0)
    .count()
        <= 1
}

const CIT_FLAGS_MESSAGE: &str = "contain only one of CachingIterator::CALL_TOSTRING, CachingIterator::TOSTRING_USE_KEY, CachingIterator::TOSTRING_USE_CURRENT, or CachingIterator::TOSTRING_USE_INNER";

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
        (
            it.flags,
            it.kind,
            it.key.as_deref().copied(),
            it.data.as_deref().copied(),
            it.inner.as_deref().copied(),
        )
    })?;
    if flags & CachingIterator::FULL_CACHE != 0
        && let (Some(key), Some(data)) = (key, data)
    {
        let data = Owner::take(req, data);
        let (key, value) = crate::keyed(req, &key, data)?;
        cache_write(req, this, |req, cache| cache.set(req, key, value))?;
    }
    if kind == Kind::RecursiveCaching
        && let Some(inner) = inner
    {
        let children = (|| -> Result<Option<Owner<PhpObject>>, Thrown> {
            let answered = crate::method(req, inner, b"hasChildren")?;
            let has = truthy(req, &answered);
            answered.give_up(req);
            let has = has?;
            if !has {
                return Ok(None);
            }
            let children = crate::method(req, inner, b"getChildren")?;
            let Some(class) = lookup_class(req, b"RecursiveCachingIterator")? else {
                children.give_up(req);
                return Ok(None);
            };
            let made = instantiate(req, class, &[*children, Value::int(flags & CIT_PUBLIC)]);
            children.give_up(req);
            made.map(Some)
        })();
        match children {
            Ok(children) => {
                let old = dual(req, this, |it| {
                    std::mem::replace(&mut it.children, children)
                })?;
                crate::give_up(req, old);
            }
            Err(thrown) if flags & CachingIterator::CATCH_GET_CHILD != 0 => {
                php_ext::raising::clear(req, thrown)?;
            }
            Err(thrown) => return Err(thrown),
        }
    }
    if flags & (CachingIterator::TOSTRING_USE_INNER | CachingIterator::CALL_TOSTRING) != 0 {
        let source = if flags & CachingIterator::TOSTRING_USE_INNER != 0 {
            inner.map_or_else(Value::null, |o| o.as_value())
        } else {
            data.unwrap_or_default()
        };
        let string = to_string(req, &source)?.into_value();
        let old = dual(req, this, |it| it.string.replace(string))?;
        crate::give_up(req, old);
    }
    dual_next(req, this, false)
}

/// The cache, or php's `BadMethodCallException` without `FULL_CACHE`.
fn full_cache(req: &mut Request, this: PhpObject) -> Result<PhpArray, Thrown> {
    let (flags, cache) = dual(req, this, |it| (it.flags, it.cache.as_deref().copied()))?;
    match cache {
        Some(cache) if flags & CachingIterator::FULL_CACHE != 0 => Ok(cache),
        _ => {
            let class = String::from_utf8_lossy(&crate::class_name(req, this.class())).into_owned();
            Err(throw_exception_ex(
                req,
                "BadMethodCallException",
                &format!("{class} does not use a full cache (see CachingIterator::__construct)"),
            ))
        }
    }
}

/// A write through the cache's owner, taken out of the iterator for the
/// write and put back whatever the write answered, so a write that
/// separates moves the owner with it; a cache made where there is none
/// yet.
fn cache_write<R>(
    req: &mut Request,
    this: PhpObject,
    write: impl FnOnce(&mut Request, &mut Owner<PhpArray>) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let mut cache = match dual(req, this, |it| it.cache.take())? {
        Some(cache) => cache,
        None => PhpArray::new(req)?,
    };
    let written = write(req, &mut cache);
    let old = dual(req, this, |it| it.cache.replace(cache))?;
    crate::give_up(req, old);
    written
}

/// The iterator a `NoRewindIterator` method steps, its receiver's
/// constructor served.
fn no_rewind_iter(req: &mut Request, this: PhpObject) -> Result<Option<Iter>, Thrown> {
    let this = checked(req, this)?;
    inner_iter(req, this)
}

fn appended(req: &mut Request, this: PhpObject) -> Result<PhpObject, Thrown> {
    match dual(req, this, |it| it.appended.as_deref().copied())? {
        Some(list) => Ok(list),
        None => Err(throw_exception_ex(
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
    let (old, old_iter) = dual(req, this, |it| (it.inner.take(), it.iter.take()))?;
    crate::give_up(req, old_iter);
    crate::give_up(req, old);
    let list = appended(req, this)?;
    if !crate::valid(req, list)? {
        return Ok(false);
    }
    let held = match crate::method(req, list, b"current")?.into_object() {
        Ok(held) => held,
        Err(current) => {
            current.give_up(req);
            return Ok(false);
        }
    };
    let next = *held;
    let old = dual(req, this, |it| it.inner.replace(held))?;
    crate::give_up(req, old);
    inner_iterate(req, this, next)?;
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

// ---- `RegexIterator` and `RecursiveRegexIterator`, over ext/pcre's pattern

/// `RegexIterator::accept`'s body on `this`: `false` for no element and
/// for an array one, and otherwise what [`regex_run`] found, turned over
/// under `INVERT_MATCH`.
fn regex_accept(req: &mut Request, this: PhpObject) -> Result<bool, Thrown> {
    let regex = regex_of(req, this)?;
    let (data, key) = dual(req, this, |it| {
        (it.data.as_deref().copied(), it.key.as_deref().copied())
    })?;
    let Some(data) = data else {
        return Ok(false);
    };
    let subject = if regex.flags.use_key() {
        key.unwrap_or_default()
    } else if data.as_array().is_some() {
        return Ok(false);
    } else {
        data
    };
    let subject = to_string(req, &subject)?;
    let matched = regex_run(req, this, &regex, *subject);
    subject.give_up(req);
    Ok(match matched? {
        Some(matched) => matched != regex.flags.inverted(),
        None => false,
    })
}

/// The receiver's `u.regex`, its constructor checked, and php's `Error`
/// where no `RegexIterator` constructor was served on it.
fn regex_of(req: &mut Request, this: PhpObject) -> Result<RegexIt, Thrown> {
    let this = checked(req, this)?;
    match dual(req, this, |it| it.regex.clone())? {
        Some(regex) => Ok(regex),
        None => Err(throw_error(
            req,
            "Error",
            "The object is in an invalid state as the parent constructor was not called",
        )),
    }
}

/// The receiver's `u.regex` changed.
fn regex_set(
    req: &mut Request,
    this: PhpObject,
    change: impl FnOnce(&mut RegexIt),
) -> Result<(), Thrown> {
    regex_of(req, this)?;
    dual(req, this, |it| it.regex.as_mut().map(change))?;
    Ok(())
}

/// The `switch` of `RegexIterator::accept`: whether the `regex_mode` found
/// what it looks for, `None` for php's `RETURN_FALSE` that
/// `INVERT_MATCH` does not turn over.
fn regex_run(
    req: &mut Request,
    this: PhpObject,
    regex: &RegexIt,
    subject: PhpString,
) -> Result<Option<bool>, Thrown> {
    let pattern = &regex.pattern;
    match regex.mode {
        Mode::Match => Ok(Some(pattern.is_match(req, subject.as_bytes()))),
        Mode::GetMatch | Mode::AllMatches => {
            regex_replace_data(req, this, None)?;
            let global = regex.mode == Mode::AllMatches;
            let (count, matches) = pattern.match_impl(req, subject, global, regex.preg_flags, 0)?;
            let found = count.as_int().is_some_and(|n| n > 0);
            count.give_up(req);
            regex_replace_data(req, this, Some(matches.into_value()))?;
            Ok(Some(found))
        }
        Mode::Split => {
            regex_replace_data(req, this, None)?;
            let pieces = pattern.split_impl(req, subject.as_bytes(), -1, regex.preg_flags)?;
            let count = pieces.as_array().map_or(0, |v| v.len());
            regex_replace_data(req, this, Some(pieces))?;
            Ok(Some(count > 1))
        }
        Mode::Replace => {
            let replacement = read_prop(req, &this, b"replacement")?.deref();
            let replacement = to_string(req, &replacement)?;
            let mut count = 0;
            let replaced =
                pattern.replace_impl(req, subject, replacement.as_bytes(), -1, &mut count);
            replacement.give_up(req);
            let Some(replaced) = replaced? else {
                return Ok(None);
            };
            let replaced = Some(replaced.into_value());
            let old = dual(req, this, |it| {
                if regex.flags.use_key() {
                    std::mem::replace(&mut it.key, replaced)
                } else {
                    std::mem::replace(&mut it.data, replaced)
                }
            })?;
            crate::give_up(req, old);
            Ok(Some(count > 0))
        }
    }
}

/// The fetched element given up for `data`, which is the object's from
/// here.
fn regex_replace_data(
    req: &mut Request,
    this: PhpObject,
    data: Option<Owner<Value>>,
) -> Result<(), Thrown> {
    let old = dual(req, this, |it| std::mem::replace(&mut it.data, data))?;
    crate::give_up(req, old);
    Ok(())
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
fn rec_checked(req: &mut Request, this: PhpObject) -> Result<PhpObject, Thrown> {
    if rec(req, this, |it| it.levels.is_empty())? {
        return Err(throw_error(
            req,
            "Error",
            "The object is in an invalid state as the parent constructor was not called",
        ));
    }
    Ok(this)
}

/// The sub-iterator's `key()`.
fn rec_key(req: &mut Request, this: PhpObject) -> Result<Owner<Value>, Thrown> {
    let Some(level) = top(req, this)? else {
        return Ok(Owner::null());
    };
    let key = level.sub.key(req)?;
    Ok(crate::dereferenced(req, key))
}

/// The sub-iterator's `current()`, what `RecursiveIteratorIterator` and a
/// `BYPASS_CURRENT` tree answer.
fn rec_current(req: &mut Request, this: PhpObject) -> Result<Owner<Value>, Thrown> {
    let Some(level) = top(req, this)? else {
        return Ok(Owner::null());
    };
    let current = level.sub.value(req)?;
    Ok(crate::dereferenced(req, current))
}

fn top(req: &mut Request, this: PhpObject) -> Result<Option<Seen>, Thrown> {
    rec(req, this, |it| it.levels.last().map(Level::seen))
}

/// Every level's views, the root's first.
fn seen_levels(req: &mut Request, this: PhpObject) -> Result<Vec<Seen>, Thrown> {
    rec(req, this, |it| it.levels.iter().map(Level::seen).collect())
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
///
/// ```php
/// class Loud extends RecursiveIteratorIterator { function nextElement(): void { throw new Exception("next"); } }
/// $quiet = new Loud(new RecursiveArrayIterator([[1, 2]]), RecursiveIteratorIterator::LEAVES_ONLY, RecursiveIteratorIterator::CATCH_GET_CHILD);
/// $quiet->next(); echo $quiet->current(), "\n";
/// $loud = new Loud(new RecursiveArrayIterator([[1, 2]]));
/// try { $loud->next(); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 1
/// next
/// ```
fn caught(req: &mut Request, this: PhpObject, thrown: Thrown) -> Result<(), Thrown> {
    if rec(req, this, |it| {
        it.flags & RecursiveIteratorIterator::CATCH_GET_CHILD != 0
    })? {
        php_ext::raising::clear(req, thrown)
    } else {
        Err(thrown)
    }
}

/// `spl_recursive_it_move_forward_ex`. A level's `valid()` that
/// re-enters `next()` and so pops its own level runs over an object the
/// step holds an owner of until the call returns, as php's
/// `GC_ADDREF(sub_object)` holds it.
///
/// ```php
/// class RIt implements RecursiveIterator {
///     public $pos = 0; public $rii; public static bool $fired = false;
///     function __construct(public array $data, public int $depth = 0) {}
///     function current(): mixed { return $this->data[$this->pos] ?? null; }
///     function key(): mixed { return $this->pos; }
///     function next(): void { $this->pos++; }
///     function rewind(): void { $this->pos = 0; }
///     function valid(): bool {
///         if ($this->rii && $this->depth === 2 && $this->pos === count($this->data) && !self::$fired) {
///             self::$fired = true;
///             $this->rii->next();
///         }
///         return $this->pos < count($this->data);
///     }
///     function hasChildren(): bool { return is_array($this->current()); }
///     function getChildren(): RecursiveIterator { $c = new RIt($this->current(), $this->depth + 1); $c->rii = $this->rii; return $c; }
/// }
/// $root = new RIt([[[1, 2], 3], 4]);
/// $rii = new RecursiveIteratorIterator($root, RecursiveIteratorIterator::SELF_FIRST);
/// $root->rii = $rii;
/// $seen = [];
/// foreach ($rii as $v) $seen[] = is_array($v) ? 'array' : $v;
/// echo implode(' ', $seen), "\n";
/// ```
/// ```expect
/// array array 1 2 3 4
/// ```
// `spl_recursive_it_move_forward_ex`'s state machine, one arm per state,
// kept whole as php keeps it.
#[allow(clippy::too_many_lines)]
fn rec_forward(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    loop {
        let Some(level) = top(req, this)? else {
            return Ok(());
        };
        let mut iterator = level.iterator;
        let mut sub = level.sub;
        let mut state = level.state;
        if state == State::Next {
            if let Err(thrown) = sub.next(req) {
                caught(req, this, thrown)?;
            }
            // `next()` may have rewound the walk, which cuts the stack
            // back to the root: the step goes on at the level it left.
            let Some(level) = top(req, this)? else {
                return Ok(());
            };
            iterator = level.iterator;
            sub = level.sub;
            state = State::Start;
        }
        let mut ended = false;
        if state == State::Start {
            let depth = rec(req, this, |it| it.levels.len())?;
            // `GC_ADDREF(sub_object)` around the call: a `next()` the
            // level's `valid()` re-enters may pop the level, and the
            // object its method runs on stands until the call returns.
            let pinned = Owner::take(req, iterator);
            let valid = sub.valid(req);
            let reentered = rec(req, this, |it| it.levels.len())
                .and_then(|n| Ok(n != depth || top(req, this)?.map(|l| l.sub) != Some(sub)));
            pinned.give_up(req);
            let valid = valid?;
            if reentered? {
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
                            answered.give_up(req);
                            has?
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
                            let next = if mode == RecursiveIteratorIterator::SELF_FIRST {
                                State::SelfFirst
                            } else {
                                State::Child
                            };
                            set_state(req, this, next)?;
                            continue;
                        }
                        if mode == RecursiveIteratorIterator::LEAVES_ONLY {
                            set_state(req, this, State::Next)?;
                            continue;
                        }
                    }
                    let hooked = hook(req, this, b"nextelement", b"nextElement");
                    set_state(req, this, State::Next)?;
                    if let Err(thrown) = hooked {
                        caught(req, this, thrown)?;
                    }
                    return Ok(());
                }
                State::SelfFirst => {
                    let mode = rec(req, this, |it| it.mode)?;
                    if mode == RecursiveIteratorIterator::SELF_FIRST
                        || mode == RecursiveIteratorIterator::CHILD_FIRST
                    {
                        hook(req, this, b"nextelement", b"nextElement")?;
                    }
                    let next = if mode == RecursiveIteratorIterator::SELF_FIRST {
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
                    let Some(held) = crate::object_answered(req, child, b"RecursiveIterator")
                    else {
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
                        if mode == RecursiveIteratorIterator::CHILD_FIRST {
                            State::SelfFirst
                        } else {
                            State::Next
                        },
                    )?;
                    let level = Level::new(req, held)?;
                    let sub = *level.sub;
                    rec(req, this, |it| it.levels.push(level))?;
                    fit_iterators(req, this)?;
                    sub.rewind(req)?;
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
            if it.levels.len() > 1 && it.levels.last().map(|l| *l.sub) == Some(sub) {
                it.levels.pop()
            } else {
                None
            }
        })?;
        if let Some(level) = popped {
            // The object before its iterator, as php's pop gives them up.
            level.iterator.give_up(req);
            level.sub.give_up(req);
            // `while (!EG(exception))`: a sub-iterator's destructor that
            // throws ends the step.
            php_ext::raising::pending(req)?;
        }
    }
}

/// `spl_recursive_it_rewind_ex`: the stack cut back to the root,
/// `endChildren` for each level left, the root rewound, and
/// `beginIteration` once per walk.
fn rec_rewind(req: &mut Request, this: PhpObject) -> Result<(), Thrown> {
    while rec(req, this, |it| it.levels.len())? > 1 {
        if let Some(level) = rec(req, this, |it| it.levels.pop())? {
            level.release(req);
        }
        hook(req, this, b"endchildren", b"endChildren")?;
    }
    fit_iterators(req, this)?;
    set_state(req, this, State::Start)?;
    if let Some(root) = top(req, this)? {
        root.sub.rewind(req)?;
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
    let levels = seen_levels(req, this)?;
    for level in levels.iter().rev() {
        if level.sub.valid(req)? {
            return Ok(true);
        }
    }
    if rec(req, this, |it| it.in_iteration)? {
        hook(req, this, b"enditeration", b"endIteration")?;
    }
    rec(req, this, |it| it.in_iteration = false)?;
    Ok(false)
}

/// What the table refuses: cloning.
const REC_REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for RecHandlers {
    fn refusals(&self) -> Refusals {
        REC_REFUSALS
    }

    /// `spl_RecursiveIteratorIterator_free_storage`: every level given
    /// up, the deepest first, while the object's members still stand, so
    /// a sub-iterator's own destructor runs before its parent's and after
    /// the class's `__destruct`, which php serves through the standard
    /// `dtor_obj`; then the members.
    ///
    /// A loop's end cuts the walk back to its root first
    /// (`spl_recursive_it_dtor`).
    ///
    /// ```php
    /// class D extends RecursiveArrayIterator { function __destruct() { echo "dtor ", count($this), "\n"; } }
    /// class R extends RecursiveIteratorIterator { function __destruct() { echo "rii\n"; } }
    /// $it = new R(new D([[1, 2]]));
    /// $it->rewind(); echo $it->current(), "\n";
    /// unset($it);
    /// foreach (new R(new D([[3], 4])) as $v) { echo $v, "\n"; break; }
    /// echo "end\n";
    /// ```
    /// ```expect
    /// 1
    /// rii
    /// dtor 2
    /// dtor 1
    /// 3
    /// dtor 1
    /// rii
    /// dtor 2
    /// end
    /// ```
    ///
    /// The levels go before the members:
    ///
    /// ```php
    /// class P { function __destruct() { echo "prop\n"; } }
    /// class A extends RecursiveArrayIterator { function __destruct() { echo "inner\n"; } }
    /// class R extends RecursiveIteratorIterator { public $p; }
    /// $o = new R(new A([1])); $o->p = new P; unset($o);
    /// ```
    /// ```expect
    /// inner
    /// prop
    /// ```
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        release_levels(req, object);
        let tree = object
            .native_mut::<RecIt>(req)
            .and_then(|it| it.tree.take());
        if let Some(tree) = tree {
            tree.release(req);
        }
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(it) = object.native::<RecIt>(req) {
            for level in &it.levels {
                visit(level.iterator.as_value());
                visit(level.sub.as_value());
            }
        }
        GcTable::Properties
    }

    /// `spl_recursive_it_get_iterator`: the walk the methods make,
    /// served in no frame of its own, so a hook that throws is called
    /// from the loop; php's `Error` over an object whose constructor
    /// never ran. `RecursiveTreeIterator` declares `key()` and
    /// `current()` itself, so `zend_implement_iterator` gives it
    /// `zend_user_it_get_iterator` and its walk serves its methods.
    ///
    /// ```php
    /// class Deep extends RecursiveIteratorIterator { function __construct() {} function endChildren(): void { throw new Exception("out"); } }
    /// try { foreach (new Deep as $v) {} } catch (Error $e) { echo count($e->getTrace()), " ", $e->getMessage(), "\n"; }
    /// class Out extends RecursiveIteratorIterator { function endChildren(): void { throw new Exception("out"); } }
    /// try { foreach (new Out(new RecursiveArrayIterator([[1]])) as $v) echo $v, "\n"; } catch (Exception $e) { echo $e->getTrace()[0]['function'], " ", count($e->getTrace()), "\n"; }
    /// foreach (new RecursiveTreeIterator(new RecursiveArrayIterator([1, [2]])) as $v) echo $v, "\n";
    /// ```
    /// ```expect
    /// 0 Object is not initialized
    /// 1
    /// endChildren 1
    /// |-1
    /// \-Array
    ///   \-2
    /// ```
    fn get_iterator(
        &self,
        req: &mut Request,
        object: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        if crate::is_a(req, object.class(), b"RecursiveTreeIterator") {
            return Ok(None);
        }
        if by_ref {
            return Err(throw_error(
                req,
                "Error",
                "An iterator cannot be used with foreach by reference",
            ));
        }
        if rec(req, object, |it| it.levels.is_empty())? {
            return Err(throw_error(req, "Error", "Object is not initialized"));
        }
        Ok(Some(Box::new(RecWalk)))
    }

    /// `spl_recursive_it_get_method`'s refusal: any method called on an
    /// object whose constructor never ran is php's `Error` naming its
    /// class, before the class's own table is asked.
    ///
    /// ```php
    /// class Lazy extends RecursiveIteratorIterator { function __construct() {} function bar() { return 1; } }
    /// $it = new Lazy;
    /// try { $it->bar(); } catch (Error $e) { echo count($e->getTrace()), " ", $e->getMessage(), "\n"; }
    /// try { $it->valid(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// 0 The Lazy instance wasn't initialized properly
    /// The Lazy instance wasn't initialized properly
    /// ```
    fn get_method(
        &self,
        req: &mut Request,
        object: PhpObject,
        lowered: &[u8],
    ) -> Result<Option<Function>, Thrown> {
        if rec(req, object, |it| it.levels.is_empty())? {
            let name =
                String::from_utf8_lossy(&crate::class_name(req, object.class())).into_owned();
            return Err(throw_error(
                req,
                "Error",
                &format!("The {name} instance wasn't initialized properly"),
            ));
        }
        Std.get_method(req, object, lowered)
    }

    /// `spl_recursive_it_get_method`: the iterator of the level the
    /// walk stands on.
    ///
    /// ```php
    /// class Tree extends RecursiveArrayIterator { function depthNote() { return "at " . json_encode($this->getArrayCopy()); } }
    /// $it = new RecursiveIteratorIterator(new Tree([1, [2, 3]]));
    /// foreach ($it as $v) echo $v, " ", $it->depthNote(), "\n";
    /// ```
    /// ```expect
    /// 1 at [1,[2,3]]
    /// 2 at [2,3]
    /// 3 at [2,3]
    /// ```
    fn method_receiver(&self, req: &mut Request, object: PhpObject) -> Option<PhpObject> {
        object
            .native::<RecIt>(req)
            .and_then(|it| it.levels.last().map(|level| *level.iterator))
    }
}

/// Every level of a recursive iterator given up, the deepest first.
fn release_levels(req: &mut Request, object: PhpObject) {
    let levels = object
        .native_mut::<RecIt>(req)
        .map(|it| std::mem::take(&mut it.levels))
        .unwrap_or_default();
    for level in levels.into_iter().rev() {
        level.release(req);
    }
    fit_iterators(req, object).ok();
}

/// `spl_recursive_it_iterator`, whose functions are the methods' own
/// walk over the object's levels (`spl_recursive_it_*_ex`); it keeps no
/// state of its own.
struct RecWalk;

impl ClassIterator for RecWalk {
    fn rewind(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        rec_rewind(req, object)
    }

    fn valid(&mut self, req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
        rec_valid(req, object)
    }

    fn current(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        rec_current(req, object)
    }

    fn key(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        rec_key(req, object)
    }

    fn move_forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        rec_forward(req, object)
    }

    /// `spl_recursive_it_dtor`: every level but the root given up, the
    /// deepest first.
    fn release(&mut self, req: &mut Request, object: PhpObject) {
        let levels = object
            .native_mut::<RecIt>(req)
            .map(|it| it.levels.split_off(it.levels.len().min(1)))
            .unwrap_or_default();
        for level in levels.into_iter().rev() {
            level.release(req);
        }
        fit_iterators(req, object).ok();
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
        let Some(held) = crate::object_answered(req, answered, b"Traversable") else {
            let name = String::from_utf8_lossy(&crate::class_name(req, given.class())).into_owned();
            return Err(throw_exception_ex(
                req,
                "LogicException",
                &format!("{name}::getIterator() must return an object that implements Traversable"),
            ));
        };
        held
    } else {
        Owner::take(req, given)
    };
    if !crate::is_a(req, root.class(), b"RecursiveIterator") {
        root.give_up(req);
        return Err(throw(
            req,
            "InvalidArgumentException",
            "An instance of RecursiveIterator or IteratorAggregate creating it is required",
        ));
    }
    let root = Level::new(req, root)?;
    let old = rec(req, this, |it| {
        let old = std::mem::take(&mut it.levels);
        it.levels.push(root);
        it.mode = mode;
        it.flags = flags;
        it.max_depth = -1;
        it.in_iteration = false;
        old
    })?;
    for level in old {
        level.release(req);
    }
    fit_iterators(req, this)?;
    Ok(())
}

/// `spl_recursive_tree_iterator_get_prefix`.
fn tree_prefix(req: &mut Request, this: PhpObject) -> Result<Vec<u8>, Thrown> {
    let levels = seen_levels(req, this)?;
    let prefix: [Vec<u8>; 6] = rec(req, this, |it| {
        std::array::from_fn(|at| {
            it.tree
                .as_ref()
                .and_then(|tree| tree.prefix.get(at))
                .map(|part| part.as_bytes().to_vec())
                .unwrap_or_default()
        })
    })?;
    let mut out = prefix[0].clone();
    let last = levels.len().saturating_sub(1);
    for (at, level) in levels.iter().enumerate() {
        let answered = crate::method(req, level.iterator, b"hasNext")?;
        let more = answered.as_bool() == Some(true);
        answered.give_up(req);
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

/// `spl_recursive_tree_iterator_get_postfix`'s bytes.
fn postfix(req: &mut Request, this: PhpObject) -> Result<Vec<u8>, Thrown> {
    rec(req, this, |it| {
        it.tree
            .as_ref()
            .map(|tree| tree.postfix.as_bytes().to_vec())
            .unwrap_or_default()
    })
}

/// `spl_recursive_tree_iterator_get_entry`: the element as a string,
/// `Array` for an array.
fn tree_entry(req: &mut Request, this: PhpObject) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(level) = top(req, this)? else {
        return Ok(None);
    };
    let data = level.sub.value(req)?;
    let bare = data.deref();
    // The string read once and given up, and the element's copy given up
    // whether the conversion answered or threw.
    let entry = if bare.as_array().is_some() {
        Ok(b"Array".to_vec())
    } else {
        with_string(req, &bare, <[u8]>::to_vec)
    };
    data.give_up(req);
    entry.map(Some)
}
