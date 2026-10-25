//! `unserialize()`, php's `php_unserialize_with_options`
//! (`Z/ext/standard/var.c:1382`) over the scanner of
//! `var_unserializer.re`, copied token for token: the var hash of every
//! slot a value was written into, in the order php pushes them, which
//! `R:n;` turns into a reference and `r:n;` reads an object back from;
//! the delayed `__unserialize()` and `__wakeup()` calls, made in the
//! order their objects were finished once the whole string is read; and
//! php's warnings, with the byte offset where the scanner stood.

use std::ops::ControlFlow;

use php_ext::abi::Passed;
use php_ext::attr::php_function;
use php_ext::call::{autoload, call, call_method, class_constant, lookup_class, new_object};
use php_ext::coerce::{to_callable, value_name};
use php_ext::raising::{
    Level, Thrown, call_stack_end, deprecated, docref, throw_exception_ex, type_error, value_error,
    warning,
};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{
    ClassId, Emalloc, Key, Layout, Owner, PhpArray, PhpObject, PhpRef, PhpString, Value, ValueRef,
};

use super::{
    Holder, INCOMPLETE_CLASS, MAGIC_MEMBER, MAGIC_MEMBER_ENUM_CASE, MAGIC_MEMBER_SERIALIZED,
    Shared, has_method, is_enum_class, is_serializable, locked, not_serializable,
};
use crate::string::Decimal;

/// `HT_MAX_SIZE` on a 64-bit build.
const HT_MAX_SIZE: i64 = 0x4000_0000;

/// Where one value of the string goes: the root the reader of that
/// depth reads into, an element of an array being filled, or a property
/// of an object being filled, by the key the string carries.
#[derive(Clone)]
enum Slot {
    /// [`VarHash::roots`]' entry of the reader that reads into it.
    Root(usize),
    /// A root its reader has handed on, php's `var_tmp_var`: an `R:`
    /// naming it later shares the value the var hash holds and writes
    /// nothing back.
    Taken,
    Element(PhpArray, Key),
    Property(PhpObject, PhpString),
    /// A place a repeated key wrote again: the slot numbered here, which
    /// holds what the place holds now, as php's pointer to the bucket
    /// reads it.
    Same(usize),
}

/// One entry of php's dtor list, which `var_destroy` walks in order: a
/// value the var hash holds an owner of until then, php's `var_tmp_var`
/// and `var_push_dtor_value`, or an object whose `__wakeup()` or
/// `__unserialize()` is delayed until the whole string is read.
enum Dtor {
    Kept(Owner<Value>),
    Wakeup(Owner<PhpObject>),
    Unserialize(Owner<PhpObject>, Owner<PhpArray>),
}

impl Dtor {
    /// The entry's owners given up, no delayed call made: an entry the
    /// list could not take.
    fn give_up(self, req: &mut Request) {
        match self {
            Dtor::Kept(value) => value.give_up(req),
            Dtor::Wakeup(object) => object.give_up(req),
            Dtor::Unserialize(object, array) => {
                object.give_up(req);
                array.give_up(req);
            }
        }
    }

    /// The zvals the entry takes of its block, `tmp_var`'s `num`: the
    /// array an `__unserialize()` is handed sits in the zval after its
    /// object's.
    const fn width(&self) -> usize {
        match self {
            Dtor::Unserialize(..) => 2,
            Dtor::Kept(_) | Dtor::Wakeup(_) => 1,
        }
    }
}

/// `VAR_ENTRIES_MAX`: the slots one `var_entries` block numbers.
const VAR_ENTRIES_MAX: usize = 1018;

/// `VAR_DTOR_ENTRIES_MAX`: the zvals one `var_dtor_entries` block holds.
const VAR_DTOR_ENTRIES_MAX: usize = 255;

/// `sizeof(var_entries)`: `used_slots`, `next` and a pointer per slot.
const VAR_ENTRIES_SIZE: usize = 16 + VAR_ENTRIES_MAX * 8;

/// `sizeof(var_dtor_entries)`: `used_slots`, `next` and a zval per entry.
const VAR_DTOR_ENTRIES_SIZE: usize = 16 + VAR_DTOR_ENTRIES_MAX * 16;

/// `sizeof(struct php_unserialize_data)`: seven words before the first
/// `var_entries` block, which it holds inline.
const UNSERIALIZE_DATA_SIZE: usize = 56 + VAR_ENTRIES_SIZE;

/// One `var_dtor_entries` block `tmp_var` `emalloc`s: its allocation and
/// the entries it holds, [`VAR_DTOR_ENTRIES_MAX`] zvals' worth at most.
struct DtorBlock {
    block: Emalloc,
    entries: Vec<Dtor>,
}

impl DtorBlock {
    /// `used_slots`.
    fn used(&self) -> usize {
        self.entries.iter().map(Dtor::width).sum()
    }
}

/// Where `tmp_var` put one entry: its block and its place in it.
#[derive(Clone, Copy)]
struct DtorAt {
    block: usize,
    entry: usize,
}

/// `php_unserialize_data`, `emalloc`'d by `php_var_unserialize_init` at
/// [`UNSERIALIZE_DATA_SIZE`] and held while the var hash stands; the
/// slots in push order, each with the value last written into it, `None`
/// once a failure has forbidden it, and the `var_entries` block `var_push`
/// `emalloc`s for each [`VAR_ENTRIES_MAX`] slots past the first, the
/// first being the data's own; the root of each reader open over it,
/// innermost last; the dtor list in its blocks; the owners of the keys
/// the slots name, which php's slots, pointers into the tables, need
/// none of; the options, which a nested `unserialize()` sets and
/// restores.
#[derive(Default)]
pub(crate) struct VarHash {
    data: Option<Emalloc>,
    slots: Vec<Option<(Slot, Value)>>,
    entries: Vec<Emalloc>,
    roots: Vec<Owner<Value>>,
    dtors: Vec<DtorBlock>,
    keys: Vec<Owner<Key>>,
    allowed: Allowed,
    /// `incomplete_classes_only` (8.5.12): every object read as an
    /// `__PHP_Incomplete_Class` without its class looked up, so no user
    /// code runs; a `C:` payload and an `E:` case kept to be written
    /// back as they were, and data that already holds one of the
    /// members this keeps them under refused.
    incomplete_classes_only: bool,
    max_depth: i64,
    depth: i64,
    /// `ref_props`: the slots of typed properties whose value was read
    /// and held to the type, which an `R:` naming them later makes a
    /// source of the cell it puts there.
    ref_props: Vec<usize>,
}

/// `allowed_classes`: every class, php's null `class_hash`, or the
/// classes whose lowered names key `class_hash`, the table
/// `php_unserialize_with_options` lays down on the request's heap for
/// the call and frees before the var hash is destroyed.
#[derive(Default)]
enum Allowed {
    #[default]
    Every,
    Only(Owner<PhpArray>),
}

impl Allowed {
    /// `unserialize_allowed_class` over a lowered name.
    fn admits(&self, lowered: &[u8]) -> bool {
        match self {
            Allowed::Every => true,
            Allowed::Only(names) => names.find(lowered).is_some(),
        }
    }

    /// `zend_hash_destroy(class_hash)` and `FREE_HASHTABLE`.
    fn free(self, req: &mut Request) {
        if let Allowed::Only(names) = self {
            names.give_up(req);
        }
    }
}

impl VarHash {
    /// `php_var_unserialize_init`'s fresh var hash: every class allowed
    /// and the depth `unserialize_max_depth` sets. Its data is asked for
    /// by [`VarHash::lay_down`] once it is the one a reader holds.
    fn fresh(req: &Request) -> VarHash {
        VarHash {
            max_depth: default_depth(req),
            ..VarHash::default()
        }
    }

    /// `php_var_unserialize_init`'s `emalloc(sizeof(struct
    /// php_unserialize_data))`, freed after `var_destroy`, which an
    /// `unserialize()`'s peak stands on; the `Err` is the memory limit's
    /// fatal.
    ///
    /// ```php
    /// class Q { public $a = 1; }
    /// $s = serialize(new Q); $e = 'O:1:"E":0:{}';
    /// unserialize($s); unserialize($e);
    /// memory_reset_peak_usage(); $m = memory_get_usage(); $x = unserialize($s);
    /// var_dump(memory_get_peak_usage() - $m, memory_get_usage() - $m);
    /// memory_reset_peak_usage(); $m = memory_get_usage(); $y = unserialize($e);
    /// var_dump(memory_get_peak_usage() - $m, memory_get_usage() - $m);
    /// ```
    /// ```expect
    /// int(12720)
    /// int(432)
    /// int(12760)
    /// int(472)
    /// ```
    fn lay_down(&mut self, req: &mut Request) -> Result<(), Thrown> {
        self.data = Some(Emalloc::new(req, UNSERIALIZE_DATA_SIZE)?);
        Ok(())
    }

    /// `var_push`: the slot numbered for `R:` and `r:`, in a fresh
    /// `var_entries` block where the last is full; the `Err` is the
    /// memory limit's fatal.
    ///
    /// An array of 1017 elements numbers 1018 slots, its own among them.
    ///
    /// ```php
    /// function peak($s) { unserialize($s); memory_reset_peak_usage(); $x = unserialize($s); return memory_get_peak_usage() - memory_get_usage(); }
    /// var_dump(peak(serialize(range(1, 1017))), peak(serialize(range(1, 1018))), peak(serialize(range(1, 2036))));
    /// ```
    /// ```expect
    /// int(12288)
    /// int(20480)
    /// int(28672)
    /// ```
    fn push(&mut self, req: &mut Request, slot: &Slot, value: Value) -> Result<usize, Thrown> {
        let at = self.slots.len();
        if at > 0 && at.is_multiple_of(VAR_ENTRIES_MAX) {
            self.entries.push(Emalloc::new(req, VAR_ENTRIES_SIZE)?);
        }
        self.slots.push(Some((slot.clone(), value)));
        Ok(at)
    }

    /// `tmp_var`: `dtor` in the last `var_dtor_entries` block, or in a
    /// fresh one where it does not fit; the `Err` is the memory limit's
    /// fatal.
    ///
    /// A delayed `__wakeup()` takes one zval and a delayed
    /// `__unserialize()` two, so 127 of those fill a block to 254 and the
    /// 128th opens another.
    ///
    /// ```php
    /// class W { function __wakeup() {} }
    /// class U { function __unserialize(array $a): void {} }
    /// function of($class, $n) { $a = []; for ($i = 0; $i < $n; $i++) { $a[] = new $class; } return serialize($a); }
    /// function peak($s) { unserialize($s); memory_reset_peak_usage(); $x = unserialize($s); return memory_get_peak_usage() - memory_get_usage(); }
    /// var_dump(peak(serialize(new W)), peak(of('W', 255)), peak(of('W', 256)), peak(of('U', 127)), peak(of('U', 128)));
    /// ```
    /// ```expect
    /// int(16384)
    /// int(16384)
    /// int(20480)
    /// int(64136)
    /// int(68608)
    /// ```
    fn tmp_var(&mut self, req: &mut Request, dtor: Dtor) -> Result<DtorAt, Thrown> {
        let fits = self
            .dtors
            .last()
            .is_some_and(|last| last.used() + dtor.width() <= VAR_DTOR_ENTRIES_MAX);
        if !fits {
            let block = match Emalloc::new(req, VAR_DTOR_ENTRIES_SIZE) {
                Ok(block) => block,
                Err(thrown) => {
                    dtor.give_up(req);
                    return Err(thrown);
                }
            };
            self.dtors.push(DtorBlock {
                block,
                entries: Vec::new(),
            });
        }
        let block = self.dtors.len() - 1;
        let entries = &mut self.dtors[block].entries;
        entries.push(dtor);
        Ok(DtorAt {
            block,
            entry: entries.len() - 1,
        })
    }

    /// The `var_tmp_var` taken at `at` holding `value`, whose owner the
    /// var hash keeps from here.
    fn fill(&mut self, req: &mut Request, at: DtorAt, value: Owner<Value>) {
        let entry = self
            .dtors
            .get_mut(at.block)
            .and_then(|block| block.entries.get_mut(at.entry));
        let left = match entry {
            Some(entry) => std::mem::replace(entry, Dtor::Kept(value)),
            None => Dtor::Kept(value),
        };
        left.give_up(req);
    }

    /// `var_push_dtor_value`: `value`'s owner the var hash's until it is
    /// destroyed, where it counts owners at all.
    fn push_dtor_value(&mut self, req: &mut Request, value: Owner<Value>) -> Result<(), Thrown> {
        if value.is_refcounted() {
            self.tmp_var(req, Dtor::Kept(value))?;
        } else {
            value.give_up(req);
        }
        Ok(())
    }

    /// `var_destroy`: the dtor list walked in order, each delayed call
    /// made with `BG(serialize_lock)` held until one of them throws, and
    /// the object of that call and of every call after it marked as
    /// destructed, so its destructor never runs; every owner given up
    /// after its entry. The answer is `read`, the outcome of the read
    /// the var hash served, or what a call threw where `read` succeeded.
    /// Where `read` failed on a throw, that throwable stands: no call is
    /// made and every object is marked, as `zend_call_function` returns
    /// without calling while a throwable is pending and leaves the
    /// return undefined, so there is never a second throwable. What a
    /// successful `read` carries is dropped where a call throws, so it
    /// carries no owner.
    ///
    /// ```php
    /// class W { function __wakeup() { throw new Exception("no"); } function __destruct() { echo "dtor W\n"; } }
    /// class V { function __wakeup() { echo "wakeup V\n"; } function __destruct() { echo "dtor V\n"; } }
    /// class U { function __unserialize(array $a): void { echo "unserialize U\n"; } function __destruct() { echo "dtor U\n"; } }
    /// try { unserialize('a:3:{i:0;O:1:"U":0:{}i:1;O:1:"W":0:{}i:2;O:1:"V":0:{}}'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// var_dump(unserialize('a:2:{i:0;O:1:"V":0:{}i:1;O:1:"V":1:{'));
    /// echo "end\n";
    /// ```
    /// ```expect
    /// unserialize U
    /// dtor U
    /// no
    ///
    /// Warning: unserialize(): Error at offset 36 of 36 bytes in %s on line 6
    /// wakeup V
    /// dtor V
    /// bool(false)
    /// end
    /// ```
    ///
    /// A class whose autoloader throws while the reader stands on it
    /// fails the read, and the `__wakeup()` queued before it is not called:
    ///
    /// ```php
    /// class A { function __wakeup() { echo "wakeup A\n"; } function __destruct() { echo "dtor A\n"; } }
    /// spl_autoload_register(function ($class) { throw new Exception("no $class"); });
    /// try { unserialize('a:2:{i:0;O:1:"A":0:{}i:1;O:1:"B":0:{}}'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// no B
    /// ```
    fn destroy<T>(self, req: &mut Request, read: Result<T, Thrown>) -> Result<T, Thrown> {
        for block in self.entries {
            block.free(req);
        }
        let mut calling = read.is_ok();
        let mut failed: Option<Thrown> = None;
        for DtorBlock { block, entries } in self.dtors {
            for dtor in entries {
                let (object, argument) = match dtor {
                    Dtor::Kept(value) => {
                        value.give_up(req);
                        continue;
                    }
                    Dtor::Wakeup(object) => (object, None),
                    Dtor::Unserialize(object, array) => (object, Some(array)),
                };
                if calling {
                    let target = *object;
                    let handed = argument.as_deref().copied();
                    let called = locked(req, |req| match handed {
                        None => call_method(req, &target, b"__wakeup", &[]),
                        Some(array) => {
                            call_method(req, &target, b"__unserialize", &[array.to_value()])
                        }
                    });
                    match called {
                        Ok(answer) => answer.give_up(req),
                        Err(thrown) => {
                            failed = Some(thrown);
                            calling = false;
                        }
                    }
                }
                if !calling {
                    object.mark_destructor_called();
                }
                object.give_up(req);
                if let Some(array) = argument {
                    array.give_up(req);
                }
            }
            block.free(req);
        }
        for key in self.keys {
            key.give_up(req);
        }
        for root in self.roots {
            root.give_up(req);
        }
        if let Some(data) = self.data {
            data.free(req);
        }
        // `failed` is only ever set while `calling`, so only over a
        // `read` that succeeded.
        match failed {
            Some(thrown) => Err(thrown),
            None => read,
        }
    }
}

/// `unserialize_max_depth`, the depth a fresh var hash is held to: the
/// quantity the entry's `OnUpdateLong` read.
///
/// `0x1` is a depth of one; `default_socket_timeout`, `file.c`'s
/// `OnUpdateLong` entry, warns where the rule interprets its value:
///
/// ```php
/// var_dump(ini_set('unserialize_max_depth', '0x1'), unserialize(serialize([[1]])), ini_set('default_socket_timeout', '5y'));
/// ```
/// ```expect
/// Warning: unserialize(): Maximum depth of 1 exceeded. The depth limit can be changed using the max_depth unserialize() option or the unserialize_max_depth ini setting in %s on line 2
///
/// Warning: unserialize(): Error at offset 14 of 24 bytes in %s on line 2
///
/// Warning: Invalid "default_socket_timeout" setting. Invalid quantity "5y": unknown multiplier "y", interpreting as "5" for backwards compatibility in %s on line 2
/// string(4) "4096"
/// bool(false)
/// string(2) "60"
/// ```
fn default_depth(req: &Request) -> i64 {
    req.ini().long(b"unserialize_max_depth")
}

/// One reader over one string, php's `p` and `max` beside the var hash
/// it reads through, which it holds as [`Holder`] says, and the depth of
/// its root among [`VarHash::roots`].
struct State<'a> {
    buf: &'a [u8],
    p: usize,
    hash: VarHash,
    holder: Holder,
    root: usize,
}

impl<'a> State<'a> {
    /// `PHP_VAR_UNSERIALIZE_INIT` over `buf`: the var hash an
    /// unserializer already open shares, where one is and
    /// `BG(serialize_lock)` is not held, a fresh one otherwise; the
    /// reader's root laid down in it. The `Err` is the memory limit's
    /// fatal, met laying a fresh one down.
    fn open(req: &mut Request, buf: &'a [u8]) -> Result<State<'a>, Thrown> {
        let fresh = VarHash::fresh(req);
        let nesting = &mut req.state::<crate::State>().var;
        let (mut hash, holder) = nesting.unserialize.open(nesting.lock, fresh);
        if holder != Holder::Joined {
            hash.lay_down(req)?;
        }
        let root = hash.roots.len();
        hash.roots.push(Owner::null());
        Ok(State {
            buf,
            p: 0,
            hash,
            holder,
            root,
        })
    }

    /// php's `var_tmp_var` for the reader's next value, taken before it
    /// is read, which [`State::hand_on`] fills; the `Err` is the memory
    /// limit's fatal.
    fn tmp_var(&mut self, req: &mut Request) -> Result<DtorAt, Thrown> {
        let null = Owner::null();
        self.hash.tmp_var(req, Dtor::Kept(null))
    }

    /// The root handed on into the `var_tmp_var` taken for it at `at`:
    /// the slots that named it name the value the var hash holds from
    /// here, and the reader's next value is a root of its own. A view,
    /// which lives as long as the var hash.
    fn hand_on(&mut self, req: &mut Request, at: DtorAt) -> Value {
        let root = self.hand_over();
        let view = *root;
        self.hash.fill(req, at, root);
        view
    }

    /// The root handed over to the caller with its owner: the slots that
    /// named it name the value as the caller keeps it.
    fn hand_over(&mut self) -> Owner<Value> {
        let root = self.take_root();
        let at = self.root;
        for (slot, _) in self.hash.slots.iter_mut().flatten() {
            if matches!(slot, Slot::Root(of) if *of == at) {
                *slot = Slot::Taken;
            }
        }
        root
    }

    /// The root taken out as the value `unserialize()` returns, with the
    /// owner the var hash held of it.
    fn take_root(&mut self) -> Owner<Value> {
        let null = Owner::null();
        match self.hash.roots.get_mut(self.root) {
            Some(root) => std::mem::replace(root, null),
            None => null,
        }
    }

    /// `php_var_unserialize`: one value into `slot`, and where it fails
    /// every slot it numbered forbidden, so nothing read later in the
    /// same var hash names a place a failed read left half made.
    fn read(&mut self, req: &mut Request, slot: Slot) -> Result<bool, Thrown> {
        let from = self.hash.slots.len();
        let read = self.value(req, Some(slot));
        if !matches!(read, Ok(true)) {
            for forbidden in self.hash.slots.iter_mut().skip(from) {
                *forbidden = None;
            }
        }
        read
    }

    /// `f`, a call that may open another unserializer, with the var hash
    /// set down where that one finds it.
    fn out<R>(&mut self, req: &mut Request, f: impl FnOnce(&mut Request) -> R) -> R {
        let hash = std::mem::take(&mut self.hash);
        let (hash, answered) = Shared::set_down(
            req,
            |nesting| &mut nesting.unserialize,
            hash,
            self.holder,
            f,
        );
        self.hash = hash;
        answered
    }

    /// `PHP_VAR_UNSERIALIZE_DESTROY`: the reader's root taken off, and
    /// the var hash handed back to the reader it is shared with, or
    /// destroyed where this was its last holder, with no delayed call
    /// made where `read`, the outcome of the read, failed on a throw
    /// ([`VarHash::destroy`]); the answer is `read`, or what a delayed
    /// call threw.
    fn close<T>(self, req: &mut Request, read: Result<T, Thrown>) -> Result<T, Thrown> {
        let State {
            mut hash,
            holder,
            root,
            ..
        } = self;
        if hash.roots.len() > root {
            for left in hash.roots.split_off(root) {
                left.give_up(req);
            }
        }
        match req
            .state::<crate::State>()
            .var
            .unserialize
            .close(hash, holder)
        {
            Some(hash) => hash.destroy(req, read),
            None => read,
        }
    }
}

/// `unserialize(string $data, array $options = []): mixed`, php's
/// `PHP_FUNCTION(unserialize)` (`Z/ext/standard/var.c:1510`).
///
/// ```php
/// var_dump(unserialize('a:3:{i:0;i:1;s:1:"k";d:1.5;i:1;a:1:{i:0;b:1;}}'), unserialize('N;'), unserialize('s:3:"a"b";'), unserialize('i:-7;'), unserialize('d:INF;'), unserialize(serialize(0.1)));
/// var_dump(unserialize(''), unserialize('b:0;'));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   int(1)
///   ["k"]=>
///   float(1.5)
///   [1]=>
///   array(1) {
///     [0]=>
///     bool(true)
///   }
/// }
/// NULL
/// string(3) "a"b"
/// int(-7)
/// float(INF)
/// float(0.1)
/// bool(false)
/// bool(false)
/// ```
///
/// Malformed data is `false` and php's warning with the offset the
/// scanner stood at; data after a whole value is warned about and the
/// value kept.
///
/// ```php
/// var_dump(unserialize('a:2:{i:0;i:1;}'), unserialize('i:1'), unserialize('s:5:"abc";'), unserialize('x'), unserialize('i:1;junk'), unserialize('a:1:{i:0;i:1;'), unserialize('i:99999999999999999999;'));
/// ```
/// ```expect
/// Warning: unserialize(): Unexpected end of serialized data in %s on line 2
///
/// Warning: unserialize(): Error at offset 13 of 14 bytes in %s on line 2
///
/// Warning: unserialize(): Error at offset 0 of 3 bytes in %s on line 2
///
/// Warning: unserialize(): Error at offset 10 of 10 bytes in %s on line 2
///
/// Warning: unserialize(): Error at offset 0 of 1 bytes in %s on line 2
///
/// Warning: unserialize(): Extra data starting at offset 4 of 8 bytes in %s on line 2
///
/// Warning: unserialize(): Error at offset 13 of 13 bytes in %s on line 2
///
/// Warning: unserialize(): Numerical result out of range in %s on line 2
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// int(1)
/// bool(false)
/// int(9223372036854775807)
/// ```
///
/// An object is made without its constructor and its properties
/// written as the string names them, mangled names placing protected
/// and private ones; `r:` is the same object again and `R:` a
/// reference to an earlier slot.
///
/// ```php
/// class P { public $a = 1; protected $b = 2; private $c = 3; function __construct() { echo "ctor\n"; } }
/// $p = unserialize(serialize(new P)); var_dump($p);
/// $o = unserialize('a:2:{i:0;O:8:"stdClass":1:{s:1:"x";i:1;}i:1;r:2;}'); var_dump($o[0] === $o[1]);
/// $r = unserialize('a:2:{i:0;i:5;i:1;R:2;}'); $r[1] = 9; var_dump($r);
/// ```
/// ```expect
/// ctor
/// object(P)#1 (3) {
///   ["a"]=>
///   int(1)
///   ["b":protected]=>
///   int(2)
///   ["c":"P":private]=>
///   int(3)
/// }
/// bool(true)
/// array(2) {
///   [0]=>
///   &int(9)
///   [1]=>
///   &int(9)
/// }
/// ```
///
/// `__unserialize()` is handed the data and `__wakeup()` is called with
/// none, each after the whole string is read, inner objects first; a
/// class that is not there, or not allowed, is `__PHP_Incomplete_Class`.
///
/// ```php
/// class U { public $d; function __unserialize(array $d): void { echo "unserialize ", json_encode($d), "\n"; $this->d = $d; } }
/// class W { public $i; function __wakeup() { echo "wakeup ", get_class($this->i ?? $this), "\n"; } }
/// $w = unserialize('O:1:"W":1:{s:1:"i";O:1:"W":1:{s:1:"i";N;}}');
/// $u = unserialize('O:1:"U":2:{s:1:"a";i:1;i:0;s:1:"b";}');
/// var_dump(unserialize('O:7:"Missing":1:{s:1:"x";i:1;}'), unserialize('O:1:"W":0:{}', ['allowed_classes' => false]));
/// ```
/// ```expect
/// wakeup W
/// wakeup W
/// unserialize {"a":1,"0":"b"}
/// object(__PHP_Incomplete_Class)#4 (2) {
///   ["__PHP_Incomplete_Class_Name"]=>
///   string(7) "Missing"
///   ["x"]=>
///   int(1)
/// }
/// object(__PHP_Incomplete_Class)#5 (1) {
///   ["__PHP_Incomplete_Class_Name"]=>
///   string(1) "W"
/// }
/// ```
///
/// The options: `allowed_classes` an array of names or a bool, and
/// `max_depth` an int, each held to its type.
///
/// ```php
/// var_dump(unserialize('O:1:"W":0:{}', ['allowed_classes' => ['w']]) instanceof W, unserialize('a:1:{i:0;a:1:{i:0;i:1;}}', ['max_depth' => 1]));
/// try { unserialize('i:1;', ['allowed_classes' => 1]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { unserialize('i:1;', ['max_depth' => -1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { unserialize('i:1;', ['allowed_classes' => ["have\0nul"]]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// class W { function __wakeup() {} }
/// ```
/// ```expect
/// Warning: unserialize(): Maximum depth of 1 exceeded. The depth limit can be changed using the max_depth unserialize() option or the unserialize_max_depth ini setting in %s on line 2
///
/// Warning: unserialize(): Error at offset 14 of 24 bytes in %s on line 2
/// bool(true)
/// bool(false)
/// unserialize(): Option "allowed_classes" must be of type array|bool, int given
/// unserialize(): Option "max_depth" must be greater than or equal to 0
/// unserialize(): Option "allowed_classes" must be an array of class names, "have" given
/// ```
///
/// An enum case is `E:` and its name, the case itself read back.
///
/// ```php
/// var_dump(unserialize('E:25:"RoundingMode:HalfEven";') === RoundingMode::HalfEven, unserialize('E:11:"Nope:Nobody";'), unserialize('E:8:"stdClass";'));
/// ```
/// ```expect
/// Warning: unserialize(): Error at offset 2 of 29 bytes in %s on line 2
///
/// Warning: unserialize(): Class 'Nope' not found in %s on line 2
///
/// Warning: unserialize(): Error at offset 0 of 19 bytes in %s on line 2
///
/// Warning: unserialize(): Invalid enum name 'stdClass' (missing colon) in %s on line 2
///
/// Warning: unserialize(): Error at offset 0 of 15 bytes in %s on line 2
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// A constant the enum declares that is no case is refused:
///
/// ```php
/// enum Foo { case Bar; const Baz = Foo::Bar; }
/// var_dump(unserialize('E:7:"Foo:Baz";'));
/// ```
/// ```expect
/// Warning: unserialize(): Foo::Baz is not an enum case in %s on line 3
///
/// Warning: unserialize(): Error at offset 14 of 14 bytes in %s on line 3
/// bool(false)
/// ```
///
/// What it answers is offered to the cycle collector as a possible root,
/// an array or an object (never a scalar or the immutable empty array),
/// so a cycle handed straight to a native, which frees its argument
/// with no root offered, is still the collector's to find:
///
/// ```php
/// $r0 = gc_status()["roots"];
/// $a = unserialize('a:1:{i:0;a:0:{}}');
/// $i = unserialize('i:1;');
/// $e = unserialize('a:0:{}');
/// echo gc_status()["roots"] - $r0, "\n";
/// var_dump(is_object(unserialize('O:8:"stdClass":1:{s:1:"b";r:1;}')));
/// echo gc_status()["roots"] - $r0, "\n";
/// ```
/// ```expect
/// 1
/// bool(true)
/// 2
/// ```
#[php_function]
fn unserialize(
    req: &mut Request,
    data: PhpString,
    #[php(default = [])] options: Passed<PhpArray>,
) -> Result<Owner<Value>, Thrown> {
    let data = data.as_bytes().to_vec();
    let options = match options {
        Passed::Given(options) => Some(options),
        Passed::Omitted => None,
    };
    php_unserialize_with_options(req, &data, options, "unserialize")
}

/// `php_unserialize_with_options`, `PHPAPI` in `php_var.h`: `data`
/// unserialised under the `allowed_classes` and `max_depth` of `options`,
/// the options' refusals naming `function`, as phar's `getMetadata()`
/// reads its archive's serialised value (design §4.10, the `standard` →
/// `phar` row); `false` for empty data, before the options are read.
/// The value comes with its one owner, the caller's.
pub fn php_unserialize_with_options(
    req: &mut Request,
    data: &[u8],
    options: Option<PhpArray>,
    function: &str,
) -> Result<Owner<Value>, Thrown> {
    if data.is_empty() {
        return Ok(Owner::bool(false));
    }
    let mut state = State::open(req, data)?;
    let given = match self::options(req, options, function) {
        Ok(given) => given,
        Err(thrown) => {
            // php's `goto cleanup`: the var hash it laid down first is
            // destroyed under the refusal.
            return state.close(req, Err(thrown));
        }
    };
    let previous = (state.hash.max_depth, state.hash.depth);
    let previous_allowed = given
        .allowed
        .map(|allowed| std::mem::replace(&mut state.hash.allowed, allowed));
    if let Some(max_depth) = given.max_depth {
        state.hash.max_depth = max_depth;
        // A nested call with a depth of its own counts from zero, for
        // itself alone.
        state.hash.depth = 0;
    }
    let root = state.root;
    // Inside another unserializer the value is read into a `var_tmp_var`.
    let tmp = if state.holder == Holder::Joined {
        Some(state.tmp_var(req)?)
    } else {
        None
    };
    let parsed = state.read(req, Slot::Root(root));
    let total = data.len();
    if let Ok(ok) = parsed {
        if !ok {
            docref(
                req,
                Level::Warning,
                &format!("Error at offset {} of {total} bytes", state.p),
            );
        } else if state.p < total {
            docref(
                req,
                Level::Warning,
                &format!("Extra data starting at offset {} of {total} bytes", state.p),
            );
        }
    }
    // Inside another unserializer the root is `var_tmp_var`'s, which the
    // var hash keeps, and the caller has a copy of it; outside, it is the
    // return value, and a failure frees it before the delayed calls.
    let answer = match (&parsed, tmp) {
        (Ok(true), Some(tmp)) => {
            let root = state.hand_on(req, tmp);
            Some(Owner::take(req, root))
        }
        // Outside, the answer is offered to the cycle collector,
        // `gc_check_possible_root(Z_COUNTED_P(return_value))`
        // (`Z/ext/standard/var.c:1481-1484`), so a cycle the caller frees
        // with no root offered, as a native's argument is freed, is still
        // the collector's to find (#2444).
        (Ok(true), None) => {
            let root = state.take_root();
            req.memory_mut().gc_possible_root(&root);
            Some(root)
        }
        _ => {
            if state.holder != Holder::Joined {
                state.take_root().give_up(req);
            }
            None
        }
    };
    // `cleanup`: this call's `class_hash` freed, the previous options
    // restored, and the var hash destroyed after them.
    if let Some(allowed) = previous_allowed {
        std::mem::replace(&mut state.hash.allowed, allowed).free(req);
    }
    let (max_depth, depth) = previous;
    state.hash.max_depth = max_depth;
    state.hash.depth = depth;
    if let Err(thrown) = state.close(req, parsed) {
        if let Some(answer) = answer {
            answer.give_up(req);
        }
        return Err(thrown);
    }
    let Some(answer) = answer else {
        return Ok(Owner::bool(false));
    };
    // A reference is unwrapped, per calling convention, after the delayed
    // calls, which may have changed what it holds.
    if let Some(cell) = answer.as_php_ref() {
        let inner = Owner::take(req, cell.get());
        answer.give_up(req);
        return Ok(inner);
    }
    Ok(answer)
}

/// `php_unserialize_data_t` with `php_var_unserialize`, `PHPAPI` in
/// `php_var.h`: one var hash across the values read out of one string,
/// each from where the last stopped, so an `R:n;` in a later value is a
/// reference to a slot an earlier one filled. What ext/session's decoders
/// read each session variable through into the session's array, and
/// what ext/spl's `unserialize()` methods read their records through.
/// Opened inside `unserialize()`, as a `C:` record's class reads its
/// payload, it shares the var hash of the string around it, so an `R:`
/// after the record names a value read inside it;
/// [`Unserializer::finish`] is `PHP_VAR_UNSERIALIZE_DESTROY`, which
/// makes the delayed `__unserialize()` and `__wakeup()` calls once the
/// last reader of the var hash is done.
///
/// ```php
/// $inner = 'x:i:1;a:0:{};m:a:0:{}';
/// var_dump(unserialize('a:2:{i:0;C:11:"ArrayObject":' . strlen($inner) . ':{' . $inner . '}i:1;R:5;}'));
/// class S implements Serializable {
///     public $d;
///     function serialize() { return serialize($this->d); }
///     function unserialize($s) { $this->d = unserialize($s); }
///     function __serialize(): array { return []; }
///     function __unserialize(array $a): void {}
/// }
/// var_dump(unserialize('a:2:{i:0;C:1:"S":19:{O:8:"stdClass":0:{}}i:1;r:3;}'));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   object(ArrayObject)#1 (1) {
///     ["storage":"ArrayObject":private]=>
///     array(0) {
///     }
///   }
///   [1]=>
///   array(0) {
///   }
/// }
/// array(2) {
///   [0]=>
///   object(S)#1 (1) {
///     ["d"]=>
///     object(stdClass)#2 (0) {
///     }
///   }
///   [1]=>
///   object(stdClass)#2 (0) {
///   }
/// }
/// ```
pub struct Unserializer<'a> {
    state: State<'a>,
}

impl<'a> Unserializer<'a> {
    /// `PHP_VAR_UNSERIALIZE_INIT` over `data`: the var hash an
    /// unserializer already open shares, or a fresh one with every class
    /// allowed and the depth `unserialize_max_depth` sets. The `Err` is
    /// the memory limit's fatal.
    pub fn new(req: &mut Request, data: &'a [u8]) -> Result<Unserializer<'a>, Thrown> {
        Ok(Unserializer {
            state: State::open(req, data)?,
        })
    }

    /// `php_var_unserialize_set_incomplete_classes_only`: whether every
    /// object is read as an `__PHP_Incomplete_Class` with no class looked
    /// up, no autoloader, `unserialize_callback_func`, `__wakeup()` or
    /// `__unserialize()` served, and a `C:` payload and an `E:` case kept
    /// for a [`Serializer`] set the same way to write back; ext/session's
    /// upload progress reads a session so before any user code may be
    /// served.
    ///
    /// [`Serializer`]: crate::Serializer
    pub fn set_incomplete_classes_only(&mut self, on: bool) {
        self.state.hash.incomplete_classes_only = on;
    }

    /// Where the scanner stands, php's `*p` after the last value.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.state.p
    }

    /// The scanner moved to `at`, as a decoder steps over the name it
    /// read itself.
    pub const fn set_offset(&mut self, at: usize) {
        self.state.p = at;
    }

    /// `php_var_unserialize` of one value into `array[key]`, the slot
    /// the var hash numbers; `false` is the scanner's failure. The key
    /// is the array's from here, as a `set` takes it. The var hash keeps
    /// an owner of what was read until it is destroyed, as php's keeps
    /// the `var_tmp_var` a session variable is read into, so an `R:` read
    /// after the array has let the value go names a place that stands.
    /// What the slot held is let go once the value is read, never
    /// before: php reads into the `var_tmp_var` and updates the slot
    /// after (`php_set_session_var`), so the objects read take no object id
    /// the old value frees.
    ///
    /// ```php
    /// class foo {}
    /// ob_start(); session_start(["save_path" => __DIR__, "use_cookies" => 0, "cache_limiter" => ""]);
    /// session_decode('baz|O:3:"foo":0:{}arr|a:1:{i:3;O:3:"foo":0:{}}');
    /// session_decode('baz|O:3:"foo":0:{}arr|a:1:{i:3;O:3:"foo":0:{}}');
    /// var_dump(spl_object_id($_SESSION["baz"]), spl_object_id($_SESSION["arr"][3]));
    /// session_destroy(); ob_end_flush();
    /// ```
    /// ```expect
    /// int(3)
    /// int(1)
    /// ```
    pub fn element(
        &mut self,
        req: &mut Request,
        array: PhpArray,
        key: Owner<Key>,
    ) -> Result<bool, Thrown> {
        let written = Key::clone(&key);
        self.state.hash.keys.push(key);
        let held = array.get(&written).map(|old| Owner::take(req, old));
        let read = match self.state.tmp_var(req) {
            Ok(tmp) => {
                let read = self.state.read(req, Slot::Element(array, written.clone()));
                if let Some(value) = array.get(&written) {
                    let value = Owner::take(req, value);
                    self.state.hash.fill(req, tmp, value);
                }
                read
            }
            Err(thrown) => Err(thrown),
        };
        if let Some(old) = held {
            old.give_up(req);
        }
        read
    }

    /// `php_var_unserialize` of one value into a `var_tmp_var`, `None`
    /// for the scanner's failure. The value is a view the var hash holds
    /// the owner of until it is destroyed, a reference unwrapped; a
    /// caller that keeps it takes an owner of its own.
    pub fn value(&mut self, req: &mut Request) -> Result<Option<Value>, Thrown> {
        let root = self.state.root;
        let tmp = self.state.tmp_var(req)?;
        let read = self.state.read(req, Slot::Root(root));
        let value = self.state.hand_on(req, tmp);
        if !read? {
            return Ok(None);
        }
        Ok(Some(value.as_php_ref().map_or(value, |v| v.get())))
    }

    /// `php_var_unserialize` of one value into a zval of the caller's
    /// own, `None` for the scanner's failure: the value is the caller's,
    /// a reference unwrapped, and what names it later names it for as
    /// long as the caller keeps it where it put it, php's `var_replace`
    /// pointing the var hash at that place. What `SplObjectStorage` reads
    /// its rows through, so a row a later one replaces is freed then.
    pub fn local(&mut self, req: &mut Request) -> Result<Option<Owner<Value>>, Thrown> {
        let root = self.state.root;
        let read = self.state.read(req, Slot::Root(root));
        let value = self.state.hand_over();
        // A read that failed, on a throw or not, frees what it left in
        // the caller's zval, as php's callers `zval_ptr_dtor` it.
        if !matches!(read, Ok(true)) {
            value.give_up(req);
            return read.map(|_| None);
        }
        let Some(cell) = value.as_php_ref() else {
            return Ok(Some(value));
        };
        let inner = Owner::take(req, cell.get());
        value.give_up(req);
        Ok(Some(inner))
    }

    /// `PHP_VAR_UNSERIALIZE_DESTROY` after the caller's read, whose
    /// outcome is `read`: where this reader was the var hash's last, the
    /// delayed calls, in the order they were queued, until one of them
    /// throws. The answer is `read`, or what a delayed call threw: where
    /// `read` failed on a throw no call is made, as php's
    /// `zend_call_function` refuses each at `EG(exception)`, so one
    /// throwable leaves. What a successful `read` carries is dropped
    /// where a call throws, so it is `Copy`: a caller keeps an owner it
    /// read beside `read`, never in it.
    ///
    /// ```php
    /// class A { function __wakeup() { echo "wakeup A\n"; throw new Exception("wake"); } }
    /// class T { public int $p; }
    /// $s = new SplObjectStorage;
    /// try { $s->unserialize('x:i:2;O:1:"A":0:{},N;;O:1:"T":1:{s:1:"p";s:1:"x";},N;;m:a:0:{}'); } catch (Throwable $e) {}
    /// try { $s->unserialize('x:i:1;O:1:"A":0:{},N;;m:a:0:{}'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// wakeup A
    /// wake
    /// ```
    pub fn finish<T: Copy>(self, req: &mut Request, read: Result<T, Thrown>) -> Result<T, Thrown> {
        self.state.close(req, read)
    }
}

/// The options `unserialize()` was given: `allowed_classes`, set
/// whenever the options are given at all, and `max_depth`.
struct Options {
    allowed: Option<Allowed>,
    max_depth: Option<i64>,
}

/// The `allowed_classes` and `max_depth` options, held to their types
/// as php holds them. `allowed_classes` given as a list or as `false` is
/// `class_hash` on the request's heap, which stands while the value is
/// read and goes before the var hash, so the peak over what is kept is
/// php's:
///
/// ```php
/// class W { function __wakeup() {} }
/// $s = serialize(new W);
/// foreach ([['W', 'Foo'], ['w'], false] as $allowed) {
///     unserialize($s, ['allowed_classes' => $allowed]);
///     memory_reset_peak_usage(); $x = unserialize($s, ['allowed_classes' => $allowed]); $after = memory_get_usage();
///     echo memory_get_peak_usage() - $after, "\n"; unset($x);
/// }
/// ```
/// ```expect
/// 17200
/// 17136
/// 12720
/// ```
fn options(req: &mut Request, given: Option<PhpArray>, function: &str) -> Result<Options, Thrown> {
    let Some(options) = given else {
        return Ok(Options {
            allowed: None,
            max_depth: None,
        });
    };
    let mut allowed = Allowed::Every;
    if let Some(classes) = options.find(b"allowed_classes") {
        let classes = classes.deref();
        match classes.as_ref() {
            ValueRef::Bool(true) => {}
            // `ALLOC_HASHTABLE` and `zend_hash_init` at the list's count,
            // or none for `false`, which no name is ever added to.
            ValueRef::Bool(false) => {
                allowed = Allowed::Only(PhpArray::new(req)?);
            }
            ValueRef::Array(list) => {
                let count = u32::try_from(list.len()).unwrap_or(u32::MAX);
                let mut names = PhpArray::init_size(req, count)?;
                if let Err(thrown) = allowed_names(req, &mut names, list, function) {
                    // `goto cleanup`, which frees what was added so far.
                    names.give_up(req);
                    return Err(thrown);
                }
                allowed = Allowed::Only(names);
            }
            _ => {
                let name = value_name(req, &classes);
                return Err(type_error(
                    req,
                    &format!(
                        "{function}(): Option \"allowed_classes\" must be of type array|bool, {name} given"
                    ),
                ));
            }
        }
    }
    let mut depth = None;
    if let Some(max) = options.find(b"max_depth") {
        let max = max.deref();
        let Some(n) = max.as_int() else {
            // `goto cleanup`: the `class_hash` made above freed.
            allowed.free(req);
            let name = value_name(req, &max);
            return Err(type_error(
                req,
                &format!("{function}(): Option \"max_depth\" must be of type int, {name} given"),
            ));
        };
        if n < 0 {
            allowed.free(req);
            return Err(value_error(
                req,
                &format!("{function}(): Option \"max_depth\" must be greater than or equal to 0"),
            ));
        }
        depth = Some(n);
    }
    Ok(Options {
        allowed: Some(allowed),
        max_depth: depth,
    })
}

/// `allowed_classes`' list into `class_hash`: each entry a string or
/// an object, its string (`zval_try_get_tmp_string`) held to
/// `zend_is_valid_class_name`, and its lowered form added
/// (`zend_hash_add_empty_element`), a copy only where the name holds an
/// upper-case letter (`zend_string_tolower`).
fn allowed_names(
    req: &mut Request,
    names: &mut Owner<PhpArray>,
    list: PhpArray,
    function: &str,
) -> Result<(), Thrown> {
    // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(classes), entry)`
    // (`Z/ext/standard/var.c:1416`), no owner of the table taken: the
    // option holds it while `__toString` runs.
    walk::foreach(req, list, Hold::None, |req, row| -> walk::Step {
        let entry = row.value().deref();
        let entry = entry.deref();
        if !matches!(entry.as_ref(), ValueRef::Str(_) | ValueRef::Object(_)) {
            let name = value_name(req, &entry);
            return Err(type_error(
                req,
                &format!(
                    "{function}(): Option \"allowed_classes\" must be an array of class names, {name} given"
                ),
            ));
        }
        let name = php_ext::coerce::to_string(req, &entry)?;
        if !valid_class_name(name.as_bytes()) {
            // php prints the name as a C string, up to its first NUL
            // byte.
            let bytes = name.as_bytes();
            let shown = String::from_utf8_lossy(php_ext::coerce::c_str(bytes)).into_owned();
            let thrown = value_error(
                req,
                &format!(
                    "{function}(): Option \"allowed_classes\" must be an array of class names, \"{shown}\" given"
                ),
            );
            name.give_up(req);
            return Err(thrown);
        }
        let lowered = if name.as_bytes().iter().any(u8::is_ascii_uppercase) {
            // `zend_string_alloc`, whatever the length.
            let bytes = name.as_bytes().to_ascii_lowercase();
            let lowered = PhpString::filled(req, bytes.len(), |out| out.copy_from_slice(&bytes));
            name.give_up(req);
            lowered?
        } else {
            name
        };
        if names.find(lowered.as_bytes()).is_some() {
            lowered.give_up(req);
        } else {
            names.set(req, lowered.into_key(), Owner::null())?;
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// `zend_is_valid_class_name`: letters, digits, `_`, `\` and the bytes
/// past 0x7f.
fn valid_class_name(name: &[u8]) -> bool {
    name.iter()
        .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'\\' || *c >= 0x80)
}

/// A scalar the key scanner read.
enum Scalar {
    Long(i64),
    Str(Vec<u8>),
    Other,
}

impl State<'_> {
    fn at(&self, at: usize) -> u8 {
        self.buf.get(at).copied().unwrap_or(0)
    }

    fn remaining(&self, from: usize) -> usize {
        self.buf.len().saturating_sub(from)
    }

    /// `uiv = [0-9]+` at `from`: its end, if one digit stands there.
    fn uiv(&self, from: usize) -> Option<usize> {
        let mut end = from;
        while self.at(end).is_ascii_digit() && end < self.buf.len() {
            end += 1;
        }
        (end > from).then_some(end)
    }

    /// `iv = [+-]? [0-9]+`.
    fn iv(&self, from: usize) -> Option<usize> {
        let start = if matches!(self.at(from), b'+' | b'-') {
            from + 1
        } else {
            from
        };
        self.uiv(start)
    }

    /// `nv = [+-]? ([0-9]* "." [0-9]+ | [0-9]+ "." [0-9]*)`.
    fn nv(&self, from: usize) -> Option<usize> {
        let start = if matches!(self.at(from), b'+' | b'-') {
            from + 1
        } else {
            from
        };
        let mut end = start;
        while self.at(end).is_ascii_digit() && end < self.buf.len() {
            end += 1;
        }
        let integral = end > start;
        if self.at(end) != b'.' || end >= self.buf.len() {
            return None;
        }
        let point = end;
        end += 1;
        while self.at(end).is_ascii_digit() && end < self.buf.len() {
            end += 1;
        }
        (integral || end > point + 1).then_some(end)
    }

    /// The longest of `iv`, `nv` and `nvexp` at `from`.
    fn number(&self, from: usize) -> Option<usize> {
        let base = [self.nv(from), self.iv(from)].into_iter().flatten().max()?;
        if matches!(self.at(base), b'e' | b'E')
            && base < self.buf.len()
            && let Some(exp) = self.iv(base + 1)
        {
            return Some(exp);
        }
        Some(base)
    }

    /// `parse_uiv`.
    fn parse_uiv(&self, from: usize) -> usize {
        let mut n: usize = 0;
        let mut at = from;
        while self.at(at).is_ascii_digit() && at < self.buf.len() {
            n = n
                .wrapping_mul(10)
                .wrapping_add(usize::from(self.at(at) - b'0'));
            at += 1;
        }
        n
    }

    /// `parse_iv2`: the number and where it ended, php's warning and the
    /// long's bound for one out of range.
    fn parse_iv2(&self, req: &mut Request, from: usize) -> (i64, usize) {
        let mut at = from;
        let mut negative = false;
        if self.at(at) == b'-' {
            negative = true;
            at += 1;
        } else if self.at(at) == b'+' {
            at += 1;
        }
        while self.at(at) == b'0' && at < self.buf.len() {
            at += 1;
        }
        let start = at;
        let mut result: u64 = 0;
        while self.at(at).is_ascii_digit() && at < self.buf.len() {
            result = result
                .wrapping_mul(10)
                .wrapping_add(u64::from(self.at(at) - b'0'));
            at += 1;
        }
        if at - start > 19 || result > i64::MAX.cast_unsigned() + u64::from(negative) {
            docref(req, Level::Warning, "Numerical result out of range");
            return (if negative { i64::MIN } else { i64::MAX }, at);
        }
        let value = if negative {
            result.wrapping_neg().cast_signed()
        } else {
            result.cast_signed()
        };
        (value, at)
    }

    /// The value written into `slot`, a sink: the slot holds the owner
    /// from here, and the var hash entry a view of it.
    fn write(
        &mut self,
        req: &mut Request,
        slot: &Slot,
        entry: Option<usize>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let view = *value;
        match slot {
            Slot::Root(at) => {
                let Some(root) = self.hash.roots.get_mut(*at) else {
                    value.give_up(req);
                    return Ok(());
                };
                let previous = std::mem::replace(root, value);
                previous.give_up(req);
            }
            // Nothing names a taken root's place any more; the `R:`
            // arm shares its value instead of writing here, and reads a
            // repeated place through the slot it follows.
            Slot::Taken | Slot::Same(_) => {
                value.give_up(req);
                return Ok(());
            }
            Slot::Element(array, key) => {
                let mut array = *array;
                let key = Owner::take(req, key.clone());
                array.set(req, key, value)?;
            }
            Slot::Property(object, name) => {
                let name = Owner::take(req, *name);
                object.unserialize_prop(req, name, value)?;
            }
        }
        if let Some(at) = entry
            && let Some(Some((_, held))) = self.hash.slots.get_mut(at)
        {
            *held = view;
        }
        Ok(())
    }

    /// `php_var_unserialize_internal` with a var hash: one value into
    /// `slot`. `false` is the scanner's failure, `self.p` where php's
    /// `*p` stands for the offset.
    // php's scanner is one function of rules; the arms stay together.
    #[allow(clippy::too_many_lines)]
    fn value(&mut self, req: &mut Request, slot: Option<Slot>) -> Result<bool, Thrown> {
        let Some(slot) = slot else {
            return Ok(false);
        };
        let start = self.p;
        if start >= self.buf.len() {
            return Ok(false);
        }
        let entry = if self.at(start) == b'R' {
            None
        } else {
            Some(self.hash.push(req, &slot, Value::null())?)
        };
        let c0 = self.at(start);
        let c1 = self.at(start + 1);
        match (c0, c1) {
            (b'R' | b'r', b':') => {
                let Some(end) = self.uiv(start + 2) else {
                    return Ok(false);
                };
                if self.at(end) != b';' || end >= self.buf.len() {
                    return Ok(false);
                }
                self.p = end + 1;
                let id = self.parse_uiv(start + 2);
                let Some(mut at) = id.checked_sub(1) else {
                    return Ok(false);
                };
                let (held_slot, held) = loop {
                    match self.hash.slots.get(at).cloned() {
                        Some(Some((Slot::Same(next), _))) if next > at => at = next,
                        Some(Some((Slot::Same(_), _)) | None) | None => return Ok(false),
                        Some(Some(entry)) => break entry,
                    }
                };
                if c0 == b'R' && matches!(held_slot, Slot::Taken) {
                    // php makes the `var_tmp_var` its caller holds a
                    // reference whose other owner goes with the var hash,
                    // so once the reader is done the element holds the
                    // value alone, which it is given here.
                    let shared = Owner::take(req, held.deref());
                    self.write(req, &slot, entry, shared)?;
                    return Ok(true);
                }
                if c0 == b'R' {
                    if slots_equal(&held_slot, &slot) {
                        return Ok(false);
                    }
                    let cell = if let Some(cell) = held.as_php_ref() {
                        cell
                    } else {
                        let owned = Owner::take(req, held);
                        let made = PhpRef::new(req, owned)?;
                        let cell = *made;
                        self.write(req, &held_slot, Some(at), made.into_value())?;
                        // `ref_props`: a typed property whose value was read
                        // becomes a source of the cell it now holds.
                        if let Slot::Property(object, name) = &held_slot
                            && self.hash.ref_props.contains(&at)
                        {
                            object.unserialize_verify(req, name.as_bytes())?;
                        }
                        cell
                    };
                    let shared = Owner::take(req, cell.as_value());
                    self.write(req, &slot, entry, shared)?;
                    return Ok(true);
                }
                if slots_equal(&held_slot, &slot) {
                    return Ok(false);
                }
                let object = held.deref();
                if object.as_object().is_none() {
                    return Ok(false);
                }
                let object = Owner::take(req, object);
                self.write(req, &slot, entry, object)?;
                Ok(true)
            }
            (b'N', b';') => {
                self.p = start + 2;
                self.write(req, &slot, entry, Owner::null())?;
                Ok(true)
            }
            (b'b', b':')
                if matches!(self.at(start + 2), b'0' | b'1')
                    && self.at(start + 3) == b';'
                    && start + 3 < self.buf.len() =>
            {
                self.p = start + 4;
                self.write(req, &slot, entry, Owner::bool(self.at(start + 2) == b'1'))?;
                Ok(true)
            }
            (b'i', b':') => {
                let Some(end) = self.iv(start + 2) else {
                    return Ok(false);
                };
                if self.at(end) != b';' || end >= self.buf.len() {
                    return Ok(false);
                }
                self.p = end + 1;
                let (n, _) = self.parse_iv2(req, start + 2);
                self.write(req, &slot, entry, Owner::int(n))?;
                Ok(true)
            }
            (b'd', b':') => self.double(req, &slot, entry, start),
            (b's' | b'S', b':') => {
                let Some(text) = self.string(req, start, c0 == b'S') else {
                    return Ok(false);
                };
                let value = PhpString::new(req, &text)?.into_value();
                self.write(req, &slot, entry, value)?;
                Ok(true)
            }
            (b'a', b':') => self.array(req, &slot, entry, start),
            (b'O' | b'C', b':') => self.object(req, &slot, entry, start, c0 == b'C'),
            (b'E', b':') => self.enum_case(req, &slot, entry, start),
            (b'}', _) => {
                docref(req, Level::Warning, "Unexpected end of serialized data");
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// The scanner without a var hash, what reads a key: a scalar, and
    /// php's failure for anything else, where php's rule leaves `*p`.
    ///
    /// ```php
    /// var_dump(unserialize('a:1:{a:0:{}i:1;}'), unserialize('a:1:{R:1;i:1;}'), unserialize('a:1:{O:8:"stdClass":0:{}i:1;}'));
    /// ```
    /// ```expect
    /// Warning: unserialize(): Error at offset 10 of 16 bytes in %s on line 2
    ///
    /// Warning: unserialize(): Error at offset 9 of 14 bytes in %s on line 2
    ///
    /// Warning: unserialize(): Error at offset 5 of 29 bytes in %s on line 2
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// ```
    fn key(&mut self, req: &mut Request) -> Option<Scalar> {
        let start = self.p;
        if start >= self.buf.len() {
            return None;
        }
        match (self.at(start), self.at(start + 1)) {
            (b'i', b':') => {
                let end = self.iv(start + 2)?;
                if self.at(end) != b';' || end >= self.buf.len() {
                    return None;
                }
                self.p = end + 1;
                Some(Scalar::Long(self.parse_iv2(req, start + 2).0))
            }
            (b's' | b'S', b':') => {
                let upper = self.at(start) == b'S';
                self.string(req, start, upper).map(Scalar::Str)
            }
            (b'N', b';') => {
                self.p = start + 2;
                Some(Scalar::Other)
            }
            (b'b', b':')
                if matches!(self.at(start + 2), b'0' | b'1') && self.at(start + 3) == b';' =>
            {
                self.p = start + 4;
                Some(Scalar::Other)
            }
            (b'd', b':') => {
                let end = self.double_end(start)?;
                self.p = end;
                Some(Scalar::Other)
            }
            // With no var hash php's rules answer failure: `a:` and the
            // references where their header ended, the objects and the
            // enum, with the rest, where they began.
            (b'a', b':') => {
                let end = self.uiv(start + 2)?;
                if self.at(end) == b':' && self.at(end + 1) == b'{' && end + 1 < self.buf.len() {
                    self.p = end + 2;
                }
                None
            }
            (b'R' | b'r', b':') => {
                let end = self.uiv(start + 2)?;
                if self.at(end) == b';' && end < self.buf.len() {
                    self.p = end + 1;
                }
                None
            }
            (b'}', _) => {
                docref(req, Level::Warning, "Unexpected end of serialized data");
                None
            }
            _ => None,
        }
    }

    /// Where a `d:` token ends, past its `;`.
    fn double_end(&self, start: usize) -> Option<usize> {
        let from = start + 2;
        for word in [&b"NAN"[..], b"INF", b"-INF"] {
            if self.buf.get(from..from + word.len()) == Some(word)
                && self.at(from + word.len()) == b';'
            {
                return Some(from + word.len() + 1);
            }
        }
        let end = self.number(from)?;
        (self.at(end) == b';' && end < self.buf.len()).then_some(end + 1)
    }

    fn double(
        &mut self,
        req: &mut Request,
        slot: &Slot,
        entry: Option<usize>,
        start: usize,
    ) -> Result<bool, Thrown> {
        let Some(end) = self.double_end(start) else {
            return Ok(false);
        };
        self.p = end;
        let text = &self.buf[start + 2..end - 1];
        let value = match text {
            b"NAN" => f64::NAN,
            b"INF" => f64::INFINITY,
            b"-INF" => f64::NEG_INFINITY,
            _ => std::str::from_utf8(text)
                .ok()
                .and_then(|t| t.parse::<f64>().ok())
                .unwrap_or(0.0),
        };
        self.write(req, slot, entry, Owner::float(value))?;
        Ok(true)
    }

    /// `"s:" uiv ":" ["]` and its `S:` twin: the bytes, `None` for the
    /// scanner's failure with `self.p` where php leaves `*p`.
    fn string(&mut self, req: &mut Request, start: usize, escaped: bool) -> Option<Vec<u8>> {
        let end = self.uiv(start + 2)?;
        if self.at(end) != b':' || self.at(end + 1) != b'"' || end + 1 >= self.buf.len() {
            return None;
        }
        let cursor = end + 2;
        let len = self.parse_uiv(start + 2);
        let maxlen = self.remaining(cursor);
        if maxlen < len {
            self.p = start + 2;
            return None;
        }
        let (text, after) = if escaped {
            let mut out = Vec::with_capacity(len);
            let mut at = cursor;
            for _ in 0..len {
                if at >= self.buf.len() {
                    return None;
                }
                if self.at(at) == b'\\' {
                    let hex = |c: u8| (c as char).to_digit(16);
                    let (Some(hi), Some(lo)) = (hex(self.at(at + 1)), hex(self.at(at + 2))) else {
                        return None;
                    };
                    out.push(u8::try_from(hi * 16 + lo).unwrap_or(0));
                    at += 3;
                } else {
                    out.push(self.at(at));
                    at += 1;
                }
            }
            (out, at)
        } else {
            (self.buf[cursor..cursor + len].to_vec(), cursor + len)
        };
        if self.at(after) != b'"' || after >= self.buf.len() {
            self.p = after;
            return None;
        }
        if self.at(after + 1) != b';' || after + 1 >= self.buf.len() {
            self.p = after + 1;
            return None;
        }
        self.p = after + 2;
        if escaped {
            docref(
                req,
                Level::Deprecated,
                "Unserializing the 'S' format is deprecated",
            );
        }
        Some(text)
    }

    /// `finish_nested_data`.
    fn finish(&mut self) -> bool {
        if self.p >= self.buf.len() || self.at(self.p) != b'}' {
            return false;
        }
        self.p += 1;
        true
    }

    /// A key the string repeats: the slots that named its place name the
    /// slot the value about to be read is numbered, which is where php's
    /// pointer to the bucket finds it; an `R:` value numbers none, and
    /// leaves them as they stand.
    ///
    /// ```php
    /// var_dump(unserialize('a:3:{i:0;i:1;i:0;a:1:{i:0;R:2;}i:1;R:2;}'));
    /// var_dump(unserialize('O:8:"stdClass":2:{s:1:"a";i:1;s:1:"a";O:8:"stdClass":1:{s:1:"b";R:2;}}'));
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   &array(1) {
    ///     [0]=>
    ///     *RECURSION*
    ///   }
    ///   [1]=>
    ///   &array(1) {
    ///     [0]=>
    ///     *RECURSION*
    ///   }
    /// }
    /// object(stdClass)#1 (1) {
    ///   ["a"]=>
    ///   &object(stdClass)#2 (1) {
    ///     ["b"]=>
    ///     *RECURSION*
    ///   }
    /// }
    /// ```
    fn repeat_place(&mut self, place: &Slot) {
        if self.at(self.p) == b'R' {
            return;
        }
        let next = self.hash.slots.len();
        for (slot, _) in self.hash.slots.iter_mut().flatten() {
            if slots_equal(slot, place) {
                *slot = Slot::Same(next);
            }
        }
    }

    /// The depth check `process_nested_*_data` opens with, and the
    /// thread's floor before it: with `max_depth` at 0 php's parser runs
    /// to the thread's end.
    fn deeper(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        call_stack_end(req)?;
        if self.hash.max_depth > 0 && self.hash.depth >= self.hash.max_depth {
            let max = self.hash.max_depth;
            docref(
                req,
                Level::Warning,
                &format!(
                    "Maximum depth of {max} exceeded. The depth limit can be changed using the max_depth unserialize() option or the unserialize_max_depth ini setting"
                ),
            );
            return Ok(false);
        }
        self.hash.depth += 1;
        Ok(true)
    }

    /// `"a:" uiv ":" "{"`: a table of the elements' count laid down as a
    /// hash, or, for none, the process's empty array, which costs
    /// nothing.
    ///
    /// ```php
    /// debug_zval_dump(unserialize('a:0:{}'));
    /// $m = memory_get_usage(); $x = unserialize('a:1:{i:0;a:0:{}}'); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// array(0) interned {
    /// }
    /// 376
    /// ```
    fn array(
        &mut self,
        req: &mut Request,
        slot: &Slot,
        entry: Option<usize>,
        start: usize,
    ) -> Result<bool, Thrown> {
        let Some(end) = self.uiv(start + 2) else {
            return Ok(false);
        };
        if self.at(end) != b':' || self.at(end + 1) != b'{' || end + 1 >= self.buf.len() {
            return Ok(false);
        }
        let (elements, _) = self.parse_iv2(req, start + 2);
        self.p = end + 2;
        if !(0..HT_MAX_SIZE).contains(&elements)
            || usize::try_from(elements).unwrap_or(usize::MAX) > self.remaining(self.p) / 2
        {
            return Ok(false);
        }
        // `array_init_size` and `zend_hash_real_init_mixed`: "we can't
        // convert from packed to hash during unserialization"; and
        // `ZVAL_EMPTY_ARRAY` for none, which costs the request nothing.
        // `array_finit_size_mixed` since 8.5.12: `return 0` where the
        // memory limit refused it.
        let made = match u32::try_from(elements) {
            Ok(size) if size > 0 => {
                let laid = req
                    .memory_mut()
                    .fallible(|req| PhpArray::sized(req, size, Layout::Hash))?;
                let Some(laid) = laid else {
                    return Ok(false);
                };
                laid
            }
            _ => Owner::take(req, PhpArray::empty()),
        };
        let array = *made;
        self.write(req, slot, entry, made.into_value())?;
        if elements == 0 {
            return Ok(self.finish());
        }
        if !self.array_elements(req, array, elements)? {
            return Ok(false);
        }
        Ok(self.finish())
    }

    /// `process_nested_array_data`.
    fn array_elements(
        &mut self,
        req: &mut Request,
        array: PhpArray,
        elements: i64,
    ) -> Result<bool, Thrown> {
        if !self.deeper(req)? {
            return Ok(false);
        }
        let done = self.array_body(req, array, elements);
        self.hash.depth -= 1;
        done
    }

    /// `process_nested_array_data`'s loop: each key read, what a key
    /// the string repeats held left to the var hash, and the value read
    /// into the element. A key's string is the var hash's until it is
    /// destroyed, and nothing of it outlives the read.
    ///
    /// ```php
    /// $s = serialize(["foo_1" => 1, "o" => (object)["p" => 2]]);
    /// $m0 = memory_get_usage();
    /// for ($i = 0; $i < 10; $i++) unserialize($s);
    /// var_dump(memory_get_usage() - $m0);
    /// ```
    /// ```expect
    /// int(0)
    /// ```
    ///
    /// Since 8.5.12 the elements stop once the memory limit was met, the
    /// unserialization failing there and the limit's fatal raised as the
    /// call returns:
    ///
    /// ```php
    /// ini_set('memory_limit', '8M');
    /// $ser = 'a:40000:{';
    /// for ($i = 0; $i < 40000; $i++) { $ser .= 'i:' . $i . ';a:1:{i:0;i:0;}'; }
    /// $u = unserialize($ser . '}');
    /// var_dump("unreached");
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 5
    /// ```
    fn array_body(
        &mut self,
        req: &mut Request,
        array: PhpArray,
        mut elements: i64,
    ) -> Result<bool, Thrown> {
        let mut array = array;
        while elements > 0 {
            elements -= 1;
            let key = match self.key(req) {
                Some(Scalar::Long(n)) => Key::Int(n),
                // A key is `zend_string_init_existing_interned`'s.
                // The var hash keeps a string key's owner, below.
                Some(Scalar::Str(bytes)) => {
                    let made = PhpString::existing_interned(req, &bytes)?;
                    // The key's own owner is the var hash's, whose slots
                    // name it until it is destroyed; each write takes one
                    // of its own.
                    let owned = Key::from_string(req, made);
                    let key = Key::clone(&owned);
                    self.hash.keys.push(owned);
                    key
                }
                _ => return Ok(false),
            };
            // `var_push_dtor_value`: a key the string repeats leaves what
            // it held to the var hash, which frees it with the rest.
            if let Some(old) = array.get(&key) {
                let old = Owner::take(req, old);
                self.hash.push_dtor_value(req, old)?;
                self.repeat_place(&Slot::Element(array, key.clone()));
            }
            let at = Owner::take(req, key.clone());
            array.set(req, at, Owner::null())?;
            if !self.value(req, Some(Slot::Element(array, key)))? {
                return Ok(false);
            }
            if elements > 0 && !matches!(self.at(self.p - 1), b';' | b'}') {
                self.p -= 1;
                return Ok(false);
            }
            // `goto failure` once the memory limit was met, since 8.5.12.
            if req.memory().has_oom() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `object ":" uiv ":" ["]`, `O:` and `C:`. A name that is no valid
    /// class name is refused before the table is asked, so an anonymous
    /// class, whose name holds a NUL, is never unserialized:
    ///
    /// ```php
    /// $name = get_class(new class {});
    /// var_dump(unserialize("O:" . strlen($name) . ":\"" . $name . "\":0:{}"));
    /// ```
    /// ```expect
    /// Warning: unserialize(): Error at offset 0 of %d bytes in %s on line 3
    /// bool(false)
    /// ```
    ///
    /// An object with no `__unserialize()` has its properties read into
    /// `Z_OBJPROP_P`, the table `rebuild_object_properties` lays down at
    /// the class's declared slots, grown by `zend_hash_extend` for the
    /// elements before one is read (`Z/ext/standard/var_unserializer.re:824-829`):
    ///
    /// ```php
    /// #[AllowDynamicProperties] class Q { public $a = 1; }
    /// class N { public $a, $b, $c, $d, $e, $f, $g, $h, $i; }
    /// #[AllowDynamicProperties] class E {}
    /// $q = serialize(new Q); $n = serialize(new N); $nine = 'O:1:"Q":9:{s:1:"a";i:1;s:1:"b";i:1;s:1:"c";i:1;s:1:"d";i:1;s:1:"e";i:1;s:1:"f";i:1;s:1:"g";i:1;s:1:"h";i:1;s:1:"i";i:1;}';
    /// foreach ([$q, $n, 'O:1:"E":0:{}', $nine] as $s) { $w = unserialize($s); unset($w); $m = memory_get_usage(); $u = unserialize($s); echo memory_get_usage() - $m, "\n"; unset($u); }
    /// ```
    /// ```expect
    /// 432
    /// 1528
    /// 96
    /// 752
    /// ```
    ///
    /// The name's copy and its lowered copy are asked for fallibly since
    /// 8.5.12; the limit is set from what the request holds, which the
    /// engines answer differently, so its figure is `%d`:
    ///
    /// ```php
    /// $name = str_repeat('A', 4 << 20);
    /// $payload = 'O:' . strlen($name) . ':"' . $name . '":0:{}';
    /// ini_set('memory_limit', memory_get_usage() + (6 << 20));
    /// var_dump(gettype(unserialize($payload)));
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of %d bytes exhausted (tried to allocate 4210688 bytes) in %s on line 5
    /// ```
    // php's rule is one block; its checks stay in its order.
    #[allow(clippy::too_many_lines)]
    fn object(
        &mut self,
        req: &mut Request,
        slot: &Slot,
        entry: Option<usize>,
        start: usize,
        custom: bool,
    ) -> Result<bool, Thrown> {
        let Some(end) = self.uiv(start + 2) else {
            return Ok(false);
        };
        if self.at(end) != b':' || self.at(end + 1) != b'"' || end + 1 >= self.buf.len() {
            return Ok(false);
        }
        let cursor = end + 2;
        let len = self.parse_uiv(start + 2);
        if self.remaining(cursor) < len || len == 0 {
            self.p = start + 2;
            return Ok(false);
        }
        let name = self.buf[cursor..cursor + len].to_vec();
        let after = cursor + len;
        if self.at(after) != b'"' {
            self.p = after;
            return Ok(false);
        }
        if self.at(after + 1) != b':' {
            self.p = after + 1;
            return Ok(false);
        }
        if name[0] == 0 || name[0] == b'\\' {
            return Ok(false);
        }
        // `zend_string_finit_existing_interned` of the name and
        // `zend_string_ftolower` of it, both asked for fallibly since
        // 8.5.12, the copy held while the lowered one is asked: `return
        // 0` where the memory limit refused either.
        let Some(held) = req
            .memory_mut()
            .fallible(|req| PhpString::existing_interned(req, &name))?
        else {
            return Ok(false);
        };
        if self.hash.incomplete_classes_only {
            // "Do not look the class up at all, so that no user code can
            // run."
            held.give_up(req);
            if !valid_class_name(&name) {
                return Ok(false);
            }
            let Some(class) = lookup_class(req, INCOMPLETE_CLASS)? else {
                return Ok(false);
            };
            return self.object_of(req, slot, entry, after, custom, class, &name, true);
        }
        let lower = name.to_ascii_lowercase();
        if lower != name {
            let lowered = req
                .memory_mut()
                .fallible(|req| PhpString::new(req, &lower))?;
            let Some(lowered) = lowered else {
                held.give_up(req);
                return Ok(false);
            };
            lowered.give_up(req);
        }
        held.give_up(req);
        let allowed = self.hash.allowed.admits(&lower);
        let mut incomplete = false;
        let class = if allowed {
            // `zend_is_valid_class_name` before the table is asked, since
            // the one class php finds there without the test, a linked
            // one, is never anonymous, whose name holds a NUL.
            if !valid_class_name(&name) {
                return Ok(false);
            }
            // The class loader and the callback run with
            // `BG(serialize_lock)` held, so what they unserialize is read
            // through a var hash of its own.
            if let Some(class) = locked(req, |req| autoload(req, &name))? {
                Some(class)
            } else {
                incomplete = true;
                if locked(req, |req| Self::callback(req, &name))? {
                    incomplete = false;
                    lookup_class(req, &name)?
                } else {
                    lookup_class(req, INCOMPLETE_CLASS)?
                }
            }
        } else {
            if !valid_class_name(&name) {
                return Ok(false);
            }
            incomplete = true;
            lookup_class(req, INCOMPLETE_CLASS)?
        };
        let Some(class) = class else {
            return Ok(false);
        };
        self.object_of(req, slot, entry, after, custom, class, &name, incomplete)
    }

    /// `object_common` and `object_custom` once the class is known: the
    /// object of `class` made and written, its data read.
    #[allow(clippy::too_many_arguments)] // php's scanner state, one each
    fn object_of(
        &mut self,
        req: &mut Request,
        slot: &Slot,
        entry: Option<usize>,
        after: usize,
        custom: bool,
        class: ClassId,
        name: &[u8],
        incomplete: bool,
    ) -> Result<bool, Thrown> {
        self.p = after;
        if let Some(shown) = not_serializable(req, class) {
            return Err(throw_exception_ex(
                req,
                "Exception",
                &format!("Unserialization of '{shown}' is not allowed"),
            ));
        }
        if custom {
            return self.custom(req, slot, entry, class, name, incomplete);
        }
        if self.p + 2 >= self.buf.len() {
            warning(req, "Bad unserialize data");
            return Ok(false);
        }
        let (elements, at) = self.parse_iv2(req, self.p + 2);
        self.p = at;
        if elements < 0
            || usize::try_from(elements).unwrap_or(usize::MAX) > self.remaining(after) / 2
        {
            return Ok(false);
        }
        if self.at(self.p) != b':' {
            return Ok(false);
        }
        if self.at(self.p + 1) != b'{' {
            self.p += 1;
            return Ok(false);
        }
        self.p += 2;
        let has_unserialize = !incomplete && has_method(req, class, b"__unserialize");
        // A `Serializable` class reads a `C:` record; an `O:` one it has no
        // `__unserialize()` for is not what its serializer wrote.
        if !has_unserialize && is_serializable(req, class) {
            let shown = String::from_utf8_lossy(
                &req.symbols()
                    .class_decl(class)
                    .map(|d| d.name.0.to_vec())
                    .unwrap_or_default(),
            )
            .into_owned();
            warning(
                req,
                &format!("Erroneous data format for unserializing '{shown}'"),
            );
            return Ok(false);
        }
        let made = new_object(req, class)?;
        let object = *made;
        self.write(req, slot, entry, made.into_value())?;
        if incomplete {
            store_member(req, object, MAGIC_MEMBER, name)?;
        }
        if has_unserialize {
            if elements >= HT_MAX_SIZE {
                return Ok(false);
            }
            // `array_init_size` and `zend_hash_real_init_mixed`, "Avoid
            // reallocation due to packed -> mixed conversion".
            // `array_finit_size_mixed` since 8.5.12.
            let size = u32::try_from(elements).unwrap_or(0);
            let laid = req
                .memory_mut()
                .fallible(|req| PhpArray::sized(req, size, Layout::Hash))?;
            let Some(array) = laid else {
                object.mark_destructor_called();
                return Ok(false);
            };
            match self.array_elements(req, *array, elements) {
                Ok(true) => {}
                Ok(false) => {
                    object.mark_destructor_called();
                    array.give_up(req);
                    return Ok(false);
                }
                Err(thrown) => {
                    array.give_up(req);
                    return Err(thrown);
                }
            }
            let held = Owner::take(req, object);
            self.hash.tmp_var(req, Dtor::Unserialize(held, array))?;
            return Ok(self.finish());
        }
        let has_wakeup = !incomplete && has_method(req, class, b"__wakeup");
        // `Z_OBJPROP_P(rval)` and `zend_hash_extend` over it: the class's
        // `get_properties` served, which may write rows of its own
        // (`php_zip_get_properties`), then the table laid down at the
        // declared slots and grown for the elements before one is read.
        // `elements` is held to half the bytes left above, far inside
        // `HT_MAX_SIZE`.
        object.handlers(req).get_properties(req, object)?;
        object.extend_properties(req, u32::try_from(elements).unwrap_or(u32::MAX))?;
        if !self.object_properties(req, object, elements)? {
            if has_wakeup {
                object.mark_destructor_called();
            }
            return Ok(false);
        }
        if has_wakeup {
            let held = Owner::take(req, object);
            self.hash.tmp_var(req, Dtor::Wakeup(held))?;
        }
        Ok(self.finish())
    }

    /// `unserialize_callback_func`: the function the ini names, handed the
    /// class name, then the class looked for again, loader and all;
    /// `true` where it stands after that, and php's warning where the
    /// function did not define it. A name that is no callable is
    /// `call_user_function`'s `Error`, which fails the read.
    ///
    /// ```php
    /// function check($name) { echo "asked for $name\n"; }
    /// ini_set('unserialize_callback_func', 'check');
    /// var_dump(unserialize('O:3:"Foo":0:{}') instanceof __PHP_Incomplete_Class);
    /// ini_set('unserialize_callback_func', 'Nonexistent');
    /// try { unserialize('O:3:"Foo":0:{}'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// asked for Foo
    ///
    /// Warning: unserialize(): Function check() hasn't defined the class it was called for in %s on line 4
    /// bool(true)
    /// Invalid callback Nonexistent, function "Nonexistent" not found or invalid function name
    /// ```
    ///
    /// The class is looked for through the autoloaders before the
    /// function is called and after, as `zend_lookup_class` looks.
    ///
    /// ```php
    /// spl_autoload_register(function ($c) { echo "autoload($c)\n"; });
    /// ini_set('unserialize_callback_func', 'cb');
    /// function cb($c) { echo "cb($c)\n"; }
    /// echo get_class(@unserialize('O:3:"Foo":0:{}')), "\n";
    /// ```
    /// ```expect
    /// autoload(Foo)
    /// cb(Foo)
    /// autoload(Foo)
    /// __PHP_Incomplete_Class
    /// ```
    fn callback(req: &mut Request, name: &[u8]) -> Result<bool, Thrown> {
        let Some(function) = req
            .ini()
            .get(b"unserialize_callback_func")
            .map(<[u8]>::to_vec)
            .filter(|f| !f.is_empty())
        else {
            return Ok(false);
        };
        let shown = String::from_utf8_lossy(&function).into_owned();
        let callee = PhpString::new(req, &function)?;
        let checked = to_callable(req, &Value::string(*callee));
        callee.give_up(req);
        let callable = checked?;
        let argument = PhpString::new(req, name)?;
        let answer = call(req, &callable, &[Value::string(*argument)]);
        argument.give_up(req);
        answer?.give_up(req);
        if autoload(req, name)?.is_some() {
            return Ok(true);
        }
        docref(
            req,
            Level::Warning,
            &format!("Function {shown}() hasn't defined the class it was called for"),
        );
        Ok(false)
    }

    /// `object_custom`, `C:`: the data's length and braces checked, then
    /// the class's unserializer over the payload, `ce->unserialize`
    /// ([`php_ext::call::class_unserialize`]), and php's warning and an
    /// object with nothing written for a class that has none.
    fn custom(
        &mut self,
        req: &mut Request,
        slot: &Slot,
        entry: Option<usize>,
        class: ClassId,
        name: &[u8],
        incomplete: bool,
    ) -> Result<bool, Thrown> {
        let (datalen, at) = self.parse_iv2(req, self.p + 2);
        self.p = at;
        if self.remaining(self.p) < 2 || self.at(self.p) != b':' {
            return Ok(false);
        }
        if self.at(self.p + 1) != b'{' {
            self.p += 1;
            return Ok(false);
        }
        self.p += 2;
        let present = i64::try_from(self.remaining(self.p)).unwrap_or(i64::MAX);
        if datalen < 0 || present <= datalen {
            warning(
                req,
                &format!(
                    "Insufficient data for unserializing - {datalen} required, {present} present"
                ),
            );
            return Ok(false);
        }
        let datalen = usize::try_from(datalen).unwrap_or(0);
        if self.at(self.p + datalen) != b'}' {
            self.p += datalen;
            return Ok(false);
        }
        let made = new_object(req, class)?;
        let object = *made;
        self.write(req, slot, entry, made.into_value())?;
        let payload = self.buf[self.p..self.p + datalen].to_vec();
        if self.hash.incomplete_classes_only {
            // `keep_payload`: "keep the payload verbatim, so that the
            // object serializes back to the same bytes", the string asked
            // for fallibly, then the class name stored.
            let kept = req
                .memory_mut()
                .fallible(|req| PhpString::new(req, &payload))?;
            let Some(kept) = kept else {
                return Ok(false);
            };
            let key = PhpString::new(req, MAGIC_MEMBER_SERIALIZED)?;
            object.unserialize_prop(req, key, kept.into_value())?;
            self.p += datalen + 1;
            store_member(req, object, MAGIC_MEMBER, name)?;
            return Ok(true);
        }
        // `zend_user_unserialize` runs outside `BG(serialize_lock)`: what
        // the class's `unserialize()` reads shares this var hash.
        let served = if incomplete {
            false
        } else {
            self.out(req, |req| {
                php_ext::call::class_unserialize(req, object, &payload)
            })?
        };
        if served {
            self.p += datalen + 1;
            return Ok(true);
        }
        let shown = String::from_utf8_lossy(
            &req.symbols()
                .class_decl(class)
                .map(|d| d.name.0.to_vec())
                .unwrap_or_default(),
        )
        .into_owned();
        warning(req, &format!("Class {shown} has no unserializer"));
        if incomplete {
            store_member(req, object, MAGIC_MEMBER, name)?;
        }
        self.p += datalen + 1;
        Ok(true)
    }

    /// `process_nested_object_data`: each key made a string, its
    /// property written `null` and then the value read into it.
    fn object_properties(
        &mut self,
        req: &mut Request,
        object: PhpObject,
        elements: i64,
    ) -> Result<bool, Thrown> {
        if !self.deeper(req)? {
            return Ok(false);
        }
        let done = self.object_body(req, object, elements);
        self.hash.depth -= 1;
        done
    }

    /// `process_nested_object_data`: each key read as
    /// `zend_string_init_existing_interned` reads it, so a property name
    /// the interned table already holds, `name` among php's own, is that
    /// string and costs nothing, and a dynamic property costs what
    /// writing it does:
    ///
    /// ```php
    /// #[AllowDynamicProperties] class P {}
    /// $o = new P; $o->name = 5; $s = serialize($o); $w = unserialize($s);
    /// $m = memory_get_usage(); $u = unserialize($s); var_dump(memory_get_usage() - $m);
    /// $m = memory_get_usage(); $n = new P; $n->name = 5; var_dump(memory_get_usage() - $m);
    /// $m = memory_get_usage(); $a = unserialize('a:1:{s:4:"name";i:5;}'); var_dump(memory_get_usage() - $m);
    /// ```
    /// ```expect
    /// int(416)
    /// int(416)
    /// int(376)
    /// ```
    ///
    /// A name the program's own compile holds is found again as well,
    /// in the table php compiles a file's names into, whether opcache
    /// keeps the file or not:
    ///
    /// ```php
    /// #[AllowDynamicProperties] class P {}
    /// $o = new P; $o->dyn = 5; $s = serialize($o); $w = unserialize($s);
    /// $m = memory_get_usage(); $u = unserialize($s); var_dump(memory_get_usage() - $m);
    /// $m = memory_get_usage(); $n = new P; $n->dyn = 5; var_dump(memory_get_usage() - $m);
    /// $m = memory_get_usage(); $a = unserialize('a:1:{s:3:"dyn";i:5;}'); var_dump(memory_get_usage() - $m);
    /// ```
    /// ```expect
    /// int(416)
    /// int(416)
    /// int(376)
    /// ```
    ///
    /// ```php
    /// #[AllowDynamicProperties] class P {}
    /// $o = new P; $o->dyn = 5; $s = serialize($o); $w = unserialize($s);
    /// $m = memory_get_usage(); $u = unserialize($s); var_dump(memory_get_usage() - $m);
    /// $m = memory_get_usage(); $n = new P; $n->dyn = 5; var_dump(memory_get_usage() - $m);
    /// $m = memory_get_usage(); $a = unserialize('a:1:{s:3:"dyn";i:5;}'); var_dump(memory_get_usage() - $m);
    /// ```
    /// ```expect regime=jit-off
    /// int(416)
    /// int(416)
    /// int(376)
    /// ```
    fn object_body(
        &mut self,
        req: &mut Request,
        object: PhpObject,
        mut elements: i64,
    ) -> Result<bool, Thrown> {
        while elements > 0 {
            elements -= 1;
            let scalar = self.key(req);
            let digits;
            let key: &[u8] = match &scalar {
                Some(Scalar::Str(bytes)) => bytes,
                Some(Scalar::Long(n)) => {
                    digits = Decimal::of(*n);
                    digits.as_bytes()
                }
                _ => return Ok(false),
            };
            // "Data that already carries one of them cannot be told apart
            // from that bookkeeping, so it is rejected rather than
            // silently overwritten."
            if self.hash.incomplete_classes_only
                && matches!(scalar, Some(Scalar::Str(_)))
                && [
                    MAGIC_MEMBER,
                    MAGIC_MEMBER_SERIALIZED,
                    MAGIC_MEMBER_ENUM_CASE,
                ]
                .contains(&key)
            {
                return Ok(false);
            }
            // A key is `zend_string_init_existing_interned`'s, so a name
            // a unit already holds costs nothing.
            let owned = PhpString::existing_interned(req, key)?;
            let name = *owned;
            self.hash.keys.push(owned.into_key());
            if let Some(old) = object.prop(req, key) {
                let old = Owner::take(req, old);
                self.hash.push_dtor_value(req, old)?;
                self.repeat_place(&Slot::Property(object, name));
            }
            let held = Owner::take(req, name);
            // A key naming a virtual property fails the whole object,
            // `is_property_visibility_changed`'s `-1`.
            if !object.unserialize_prop(req, held, Owner::null())? {
                return Ok(false);
            }
            let at = self.hash.slots.len();
            if !self.value(req, Some(Slot::Property(object, name)))? {
                return Ok(false);
            }
            object.unserialize_verify(req, key)?;
            // An `R:` value numbered no slot of its own.
            if matches!(self.hash.slots.get(at), Some(Some((Slot::Property(of, _), _))) if *of == object)
            {
                self.hash.ref_props.push(at);
            }
            if elements > 0 && !matches!(self.at(self.p - 1), b';' | b'}') {
                self.p -= 1;
                return Ok(false);
            }
            // `goto failure` once the memory limit was met, since 8.5.12.
            if req.memory().has_oom() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `"E:" uiv ":" ["]`: `Enum:Case`, the case read back.
    ///
    /// The class named is `zend_lookup_class`, which asks the autoloaders.
    ///
    /// ```php
    /// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'Lazy') { enum Lazy { case A; } } });
    /// var_dump(unserialize('E:6:"Lazy:A";'));
    /// ```
    /// ```expect
    /// load Lazy
    /// enum(Lazy::A)
    /// ```
    fn enum_case(
        &mut self,
        req: &mut Request,
        slot: &Slot,
        entry: Option<usize>,
        start: usize,
    ) -> Result<bool, Thrown> {
        let Some(end) = self.uiv(start + 2) else {
            return Ok(false);
        };
        if self.at(end) != b':' || self.at(end + 1) != b'"' || end + 1 >= self.buf.len() {
            return Ok(false);
        }
        let cursor = end + 2;
        let len = self.parse_uiv(start + 2);
        if self.remaining(cursor) < len || len == 0 {
            self.p = start + 2;
            return Ok(false);
        }
        let text = self.buf[cursor..cursor + len].to_vec();
        let after = cursor + len;
        if self.at(after) != b'"' {
            self.p = after;
            return Ok(false);
        }
        if self.at(after + 1) != b';' {
            self.p = after + 1;
            return Ok(false);
        }
        let shown = String::from_utf8_lossy(&text).into_owned();
        let Some(colon) = text.iter().position(|b| *b == b':') else {
            docref(
                req,
                Level::Warning,
                &format!("Invalid enum name '{shown}' (missing colon)"),
            );
            return Ok(false);
        };
        let (enum_name, case) = (&text[..colon], &text[colon + 1..]);
        let enum_shown = String::from_utf8_lossy(enum_name).into_owned();
        let case_shown = String::from_utf8_lossy(case).into_owned();
        if !valid_class_name(enum_name) {
            return Ok(false);
        }
        if self.hash.incomplete_classes_only {
            // "Do not load the enum: keep the case name, so that the value
            // serializes back to the same bytes."
            self.p = after + 2;
            let Some(class) = lookup_class(req, INCOMPLETE_CLASS)? else {
                return Ok(false);
            };
            let made = new_object(req, class)?;
            let object = *made;
            self.write(req, slot, entry, made.into_value())?;
            if req.memory().has_oom() {
                return Ok(false);
            }
            store_member(req, object, MAGIC_MEMBER, enum_name)?;
            store_member(req, object, MAGIC_MEMBER_ENUM_CASE, case)?;
            return Ok(true);
        }
        let Some(class) = autoload(req, enum_name)? else {
            docref(
                req,
                Level::Warning,
                &format!("Class '{enum_shown}' not found"),
            );
            return Ok(false);
        };
        if !is_enum_class(req, class) {
            docref(
                req,
                Level::Warning,
                &format!("Class '{enum_shown}' is not an enum"),
            );
            return Ok(false);
        }
        self.p = after + 2;
        let found = req.symbols().class_decl(class).and_then(|decl| {
            decl.constants
                .iter()
                .find(|c| *c.name.0 == *case)
                .map(|c| c.is_case)
        });
        match found {
            None => {
                docref(
                    req,
                    Level::Warning,
                    &format!("Undefined constant {enum_shown}::{case_shown}"),
                );
                return Ok(false);
            }
            // `ZEND_CLASS_CONST_IS_CASE`: a constant the enum declares
            // that is no case is refused, whatever it holds.
            Some(false) => {
                docref(
                    req,
                    Level::Warning,
                    &format!("{enum_shown}::{case_shown} is not an enum case"),
                );
                return Ok(false);
            }
            Some(true) => {}
        }
        let value = class_constant(req, class, case)?;
        self.write(req, slot, entry, value)?;
        Ok(true)
    }
}

/// `zend_hash_str_update(Z_OBJPROP_P(rval), member, ...)` with a string
/// of `value`, as `php_store_class_name` stores an incomplete object's
/// class name and an unserializer reading incomplete classes only its
/// kept `E:` case.
fn store_member(
    req: &mut Request,
    object: PhpObject,
    member: &[u8],
    value: &[u8],
) -> Result<(), Thrown> {
    let key = PhpString::new(req, member)?;
    let stored = match PhpString::new(req, value) {
        Ok(stored) => stored.into_value(),
        Err(thrown) => {
            key.give_up(req);
            return Err(thrown);
        }
    };
    object.unserialize_prop(req, key, stored)?;
    Ok(())
}

/// Whether two slots name the same place, php's `rval_ref == rval`.
fn slots_equal(left: &Slot, right: &Slot) -> bool {
    match (left, right) {
        (Slot::Root(one), Slot::Root(other)) => one == other,
        (Slot::Taken, Slot::Taken) => true,
        (Slot::Element(one, key), Slot::Element(other, key2)) => one == other && key == key2,
        (Slot::Property(one, name), Slot::Property(other, name2)) => {
            one == other && name.as_bytes() == name2.as_bytes()
        }
        _ => false,
    }
}

#[allow(dead_code)]
fn unused(req: &mut Request) {
    deprecated(req, "");
}
