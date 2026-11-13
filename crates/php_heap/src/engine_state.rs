//! What may stand in engine state (ADR-0005; #4348 ruling 2): a value
//! every request thread reads at once, so one that holds no pointer into a
//! request's heap. A request's string, array, object, reference or
//! resource moves with the request that owns it and is touched by its
//! thread alone; one kept engine-wide would be read by another request's
//! thread while its owner frees or writes it. `Send + Sync` cannot say
//! so, since [`Zval`] is both for the frozen values a published unit
//! shares, so [`EngineState`] says it, and each slot the engine keeps a
//! value engine-wide in, an extension's entries and the stream layer's
//! state beside them, is bounded by it and by `Sync` beside it.
//! The trait asks `Send` and not `Sync` (#4474): a value behind a `Mutex`,
//! an idle entry `api::Pool` keeps, is moved and never shared, and is
//! engine state all the same; where a value is shared, through an `Arc`,
//! a `&'static` or one of those slots, `Sync` is asked there.
//!
//! A type is engine state by `#[derive(EngineState)]`, which holds every
//! field to the bound, or by an impl written by hand where `unsafe` may
//! stand (the homes `unsafe.homes` names), for a leaf the derive cannot
//! read through: here, the primitives, the standard containers over engine
//! state, a `Cow` and a `Range` among them, an owned fd, a file and the standard
//! library's sockets, a function pointer, [`Bytes`], a [`ClassCache`], a
//! [`ZvalType`], a debug build's `Origin`, [`Strings`], [`FrozenZval`]
//! and [`AnyState`](crate::AnyState).
//! An `AnyState` is a value of engine state behind its erased type, which
//! the engine holds where it keeps a value it does not name, the compiled
//! tier's for an epoch (#4492): its one constructor takes engine state,
//! so a field of it is held to the bound where the value is made and not
//! where it is kept. In an extension's `src/ffi/`, an impl by hand stands
//! for a C library's object, beside its `Send` or `Sync` argument, which
//! is the homes' prose and no check: there one stands for any type. The
//! seal is the `engine-state` rule's sensor in `xtask api-check`. It
//! reads the syntax of the sources a crate under `crates/`, `ext/` and
//! `sapi/` compiles, and of what they reach by `mod`, `#[path]` and
//! `include!`, a home covering the crate's own files only. It refuses a hand-written impl outside the homes whose
//! trait path ends in the trait's name or one a `use .. as` gives it, in
//! source or in a macro's tokens, and each place it cannot read: a macro's
//! impl whose trait is a metavariable or a path a repetition builds, in a
//! crate that names the trait; the trait handed to a macro defined
//! elsewhere; an `include!` or `#[path]` it cannot follow; and a string
//! naming the trait in the build's closure beside an `OUT_DIR` include. The
//! derive's own expansion is exempt for every macro token in it. A
//! `build.rs` that writes the impl without naming the trait, an impl a proc
//! macro parses from a string, a local macro that forwards the trait to a
//! macro outside the workspace, and a rename another crate exports stand
//! outside what it reads (#4463). [`Zval`] and every pointer into a
//! request's heap have no impl.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::{CString, OsString};
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

use crate::{Arr, Bytes, ClassCache, Fatal, Intern, Key, Strings, Zval, ZvalType};

/// A value engine state may hold: kept by the engine across requests
/// and holding no pointer into a request's heap, at any depth.
///
/// What it promises: nothing reachable from the value is a request's, no
/// [`Zval`] a request made, no request string, array, object, reference
/// or resource, and no pointer into a request's heap.
/// `#[derive(EngineState)]` makes the promise by holding every field to
/// this bound; a hand-written impl in a home makes it for a leaf the
/// derive cannot read through. It asks `Send` and not `Sync`: where every
/// request reads the value at once, the entry that shares it asks `Sync`
/// beside it.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot stand in engine state: it may hold a handle into a request's heap",
    label = "not `EngineState`",
    note = "engine state is shared by every request (ADR-0005); a type is engine state by `#[derive(EngineState)]`, which holds each field to this bound"
)]
pub trait EngineState: Send + 'static {}

/// Each listed type is engine state as it stands.
macro_rules! leaves {
    ($($leaf:ty),* $(,)?) => {$(
        // A primitive or a value of the standard library that owns no
        // pointer a request could have made; an fd or a socket is a
        // number into the process's table.
        impl EngineState for $leaf {}
    )*};
}

leaves! {
    (), bool, char, u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64,
    str, String, NonZeroU32, PathBuf, OsString, CString, Duration, Instant, SystemTime,
    AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, AtomicI64, std::hash::RandomState,
    std::fs::File, std::net::TcpStream, std::net::TcpListener, std::net::UdpSocket,
}

#[cfg(unix)]
leaves! {
    std::os::fd::OwnedFd, std::os::unix::net::UnixStream, std::os::unix::net::UnixListener,
    std::os::unix::net::UnixDatagram,
}

/// Each listed container is engine state where what it holds is.
macro_rules! containers {
    ($(<$($held:ident $(: ?$sized:ident)?),*> $container:ty),* $(,)?) => {$(
        // The container owns what it holds and no pointer beside it, and
        // each thing it holds is engine state by its own bound.
        impl<$($held: $(?$sized +)? EngineState),*> EngineState for $container {}
    )*};
}

containers! {
    <T> Option<T>,
    <T, E> Result<T, E>,
    <T: ?Sized> Box<T>,
    <T> [T],
    <T> Vec<T>,
    <T> VecDeque<T>,
    <T> BTreeSet<T>,
    <K, V> BTreeMap<K, V>,
    <T, S> HashSet<T, S>,
    <K, V, S> HashMap<K, V, S>,
    <T: ?Sized> Mutex<T>,
    <T: ?Sized> RwLock<T>,
    <T> OnceLock<T>,
    <T> std::ops::Range<T>,
    <A, B> (A, B),
    <A, B, C> (A, B, C),
    <A, B, C, D> (A, B, C, D),
}

// A share of what it points at, which is engine state by its own bound
// and `Sync`, since every holder of the share reads it at once.
impl<T: ?Sized + EngineState + Sync> EngineState for Arc<T> {}

// A borrow every holder reads at once, of what is engine state by its
// own bound and `Sync`.
impl<T: ?Sized + EngineState + Sync> EngineState for &'static T {}

// An array owns its elements and nothing beside them, each engine state
// by the bound.
impl<T: EngineState, const N: usize> EngineState for [T; N] {}

// A borrow every holder reads at once, of what is engine state by its own
// bound and `Sync`, or what it owns, engine state by its own bound: each
// arm is a value the impls above vouch for, and nothing beside it.
impl<B> EngineState for Cow<'static, B>
where
    B: ?Sized + ToOwned + EngineState + Sync,
    B::Owned: EngineState,
{
}

// A `PhantomData` holds nothing at all.
impl<T: ?Sized + Send + 'static> EngineState for PhantomData<T> {}

/// Each listed function pointer's type is engine state whatever it takes
/// and answers: a function pointer is code the binary holds and carries no
/// value. `#[derive(EngineState)]` passes a field written as `fn(..)`
/// unread, so a shape is listed here only for a type alias of one held
/// where the derive reads it, `Option<BufferInit>`, one line a shape.
/// A by-value parameter where another listed shape borrows would make the
/// two overlap (rustc's `coherence_leak_check`): such a field spells its
/// `fn(..)` out instead.
macro_rules! code {
    ($(<$($p:ident $(: ?$sized:ident)?),*> fn($($arg:ty),*)),* $(,)?) => {$(
        // A function pointer is an address of the binary's code and holds
        // no data.
        impl<R: 'static, $($p: $(?$sized +)? 'static),*> EngineState for fn($($arg),*) -> R {}
    )*};
}

code! {
    <A: ?Sized, B> fn(&mut A, B),
    <A: ?Sized, B: ?Sized, C, D> fn(&mut A, &B, C, D),
}

// The bytes are the binary's, a box's or a share of a published block
// (`bytes.rs`), never a request's heap.
impl EngineState for Bytes {}

// A class cache is a number, an offset into a request's map table; the
// class each request finds there is the request's, and stands in its
// table, never here (`string.rs`).
impl EngineState for ClassCache {}

// A zval's type is a byte, php's `IS_*`, which names a kind of value and
// holds none (`zval.rs`).
impl EngineState for ZvalType {}

// An origin is a number, an extension's `module_number`, which a debug
// build's heap keeps beside a block and which names no block itself
// (`sensed.rs`).
#[cfg(debug_assertions)]
impl EngineState for crate::Origin {}

/// The engine's permanent table (`string.rs`, #4633's ruling): the
/// blocks it holds are its own, laid off every request's heap, and
/// immutable once it is published. A string of it is "immutable, hashed
/// before it is published, never refcounted and never freed while the
/// engine stands", laid in its bump or by a set a cache-bound compile
/// filled on the system allocator ([`crate::Pending`]); a constant array
/// of it is one `persistent_array` laid on the system allocator, marked
/// `IS_ARRAY_IMMUTABLE`, and freed with the table. Each carries
/// `IS_STR_PERMANENT`'s bit from the moment the table holds it, a set's
/// as [`Strings::publish`] takes the set in (#4656). The pointers it holds
/// are to those blocks and its bump's chunks, beside the class caches'
/// numbers. A constant array's keys and elements are held to the table's
/// values as it is laid (#4655): the table's [`Intern::constant_array`]
/// refuses a key or an element [`FrozenZval::of`] refuses, and a set's
/// ([`crate::Pending::constant_array`]) one that is neither that nor a
/// string or array of the set, which [`Strings::publish`] makes the
/// table's with the array. Its `Send` and `Sync` impls make the
/// same argument for moving and sharing it: the only other names for
/// its strings and arrays are the zvals of the units the engine
/// published, which read and never write them, and growing it takes
/// `&mut self`.
impl EngineState for Strings {}

/// A value no request owns: a scalar, a string of the engine's
/// permanent table, [`Strings`], or one of that table's constant arrays,
/// immutable, whose every key and element is one of these in turn. It
/// is how engine state holds a value where a [`Zval`] could be a
/// request's: `Engine::constant_values` is one per constant, made where
/// the engine publishes the constant at `MINIT`, and a request reads it
/// as the [`Zval`] it is. Made from a scalar, from the table
/// ([`FrozenZval::interned`]), or from a value [`FrozenZval::of`] has
/// read through, so no request's value can be one.
///
/// One thing neither constructor can check (#4656): the table whose
/// block a frozen value names outlives the value. The bits a value
/// carries say a table laid it and published it, never which table or
/// for how long, so a frozen value is held no longer than its table, and
/// the engine's tables keep that by living as long as the engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrozenZval(Zval);

impl FrozenZval {
    /// `null`.
    #[must_use]
    pub const fn null() -> FrozenZval {
        FrozenZval(Zval::null())
    }

    /// A boolean.
    #[must_use]
    pub const fn bool(b: bool) -> FrozenZval {
        FrozenZval(Zval::bool(b))
    }

    /// An integer.
    #[must_use]
    pub const fn long(n: i64) -> FrozenZval {
        FrozenZval(Zval::long(n))
    }

    /// A float.
    #[must_use]
    pub const fn double(f: f64) -> FrozenZval {
        FrozenZval(Zval::double(f))
    }

    /// The string of `bytes` the permanent table holds, interned there on
    /// the first ask, and the fatal the table raised where it had no room:
    /// laid in the table's bump, or the frozen table's or one the table
    /// holds, each published. Held no longer than `strings` stands
    /// ([`FrozenZval`]).
    pub fn interned(strings: &mut Strings, bytes: &[u8]) -> Result<FrozenZval, Fatal> {
        strings.intern(bytes).map(|s| FrozenZval(Zval::string(s)))
    }

    /// `z`, where no request owns it or anything it reaches, and `None`
    /// where a request may: what a walk over a value the engine
    /// publishes asks of each value it reaches (#4633's ruling, #4649).
    /// Each type is decided by name, so a type [`ZvalType`] gains does
    /// not compile until it is decided here:
    ///
    /// - `null`, a boolean, an integer and a float hold no pointer, and
    ///   are admitted;
    /// - a string is the table's where it carries the bits a published
    ///   string of a table carries, `IS_STR_PERMANENT` among them: the
    ///   frozen table's and the table's own are laid with them, a
    ///   cache-bound compile's set ([`crate::Pending`]) lays its strings
    ///   without the last and [`Strings::publish`] sets it as the set
    ///   enters the table, and a request's heap lays none of them on a
    ///   string. A string a request's compile made in the request's
    ///   memory, and one of a set not yet published, is refused;
    /// - an array is the table's where it is `IS_ARRAY_IMMUTABLE` and
    ///   carries `IS_STR_PERMANENT`'s bit, which only a constant array
    ///   the table holds and the process's empty array do (`array.rs`),
    ///   a set's taking the bit as the set is published, and is admitted
    ///   where each key and element is in turn. A table's homes refuse
    ///   to lay an array holding anything else ([`Intern::constant_array`],
    ///   #4655), and each is read here all the same, an array named
    ///   twice once, so that what this admits rests on no maker;
    /// - `UNDEF`, a constant expression's index (`IS_CONSTANT_AST`) and an
    ///   `IS_PTR`'s are refused, at the top and as an element (#4656):
    ///   an index means something only beside the table it names, which
    ///   a frozen value read by any request outside its unit is not, so
    ///   deciding one against its own table is the walk's over the unit
    ///   (#4649), and an `UNDEF` is a value no publisher is shown to
    ///   write;
    /// - an object, a resource, a reference and an `IS_INDIRECT` are a
    ///   request's or point at what may be, and are refused.
    ///
    /// What it admits names its table's blocks, and is held no longer
    /// than that table stands ([`FrozenZval`]).
    #[must_use]
    pub fn of(z: Zval) -> Option<FrozenZval> {
        let mut walk = Walk::default();
        if !walk.admits(z) {
            return None;
        }
        while let Some(a) = walk.unread.pop() {
            for (key, element) in &a {
                if !FrozenZval::admits_key(key) || !walk.admits(element) {
                    return None;
                }
            }
        }
        Some(FrozenZval(z))
    }

    /// Whether [`FrozenZval::of`] admits `z` as it stands, before it
    /// reads an array's keys and elements: a scalar, or a string or a
    /// constant array that carries the bits a table's published one
    /// does. Each type is decided here by name.
    pub(crate) fn admits_standing(z: Zval) -> bool {
        match z.kind() {
            ZvalType::Null
            | ZvalType::False
            | ZvalType::True
            | ZvalType::Long
            | ZvalType::Double => true,
            ZvalType::String => z.as_str().is_some_and(crate::Str::is_permanent),
            ZvalType::Array => z.as_arr().is_some_and(Arr::is_permanent),
            ZvalType::Undef
            | ZvalType::ConstExpr
            | ZvalType::Ptr
            | ZvalType::Object
            | ZvalType::Resource
            | ZvalType::Reference
            | ZvalType::Indirect => false,
        }
    }

    /// Whether [`FrozenZval::of`] admits `key` as a constant array's key:
    /// an integer, or a string as [`FrozenZval::admits_standing`] admits
    /// one.
    pub(crate) fn admits_key(key: Key) -> bool {
        match key {
            Key::Long(_) => true,
            Key::Str(s) => FrozenZval::admits_standing(Zval::string(s)),
        }
    }

    /// The value, for a request to read: a scalar, an interned string or
    /// a constant array, which no request counts.
    #[must_use]
    pub const fn zval(self) -> Zval {
        self.0
    }
}

/// The constant arrays [`FrozenZval::of`] has met and not yet read
/// through, and every one it has met, so an array a value names twice,
/// at any depth, is read once.
#[derive(Default)]
struct Walk {
    unread: Vec<Arr>,
    met: HashSet<std::ptr::NonNull<u8>>,
}

impl Walk {
    /// Whether `z` is admitted as it stands
    /// ([`FrozenZval::admits_standing`]), an array that is put on the
    /// walk the first time it is met, for its keys and elements to be
    /// read.
    fn admits(&mut self, z: Zval) -> bool {
        if !FrozenZval::admits_standing(z) {
            return false;
        }
        if let Some(a) = z.as_arr()
            && self.met.insert(a.address())
        {
            self.unread.push(a);
        }
        true
    }
}

// A frozen value is a scalar, a published string of the permanent
// table, which is immutable, never refcounted and alive while the engine
// stands (`Strings`), or a published constant array of that table, whose
// keys and elements `FrozenZval::of` read to be the same; its
// constructors make nothing else.
impl EngineState for FrozenZval {}

#[cfg(test)]
mod tests {
    use super::{EngineState, FrozenZval};
    use crate::{Arr, Heap, Intern, Key, Strings, Zval, ZvalType};

    fn held<T: ?Sized + EngineState>() {}

    #[test]
    fn the_leaves_and_containers_the_bundle_keeps_are_engine_state() {
        held::<Option<Vec<(Box<[u8]>, std::sync::Arc<str>)>>>();
        held::<std::collections::HashMap<Box<[u8]>, std::sync::Arc<FrozenZval>>>();
        held::<&'static [&'static str]>();
        held::<fn(&mut String, &[u8], i64, bool) -> Result<Vec<u8>, ()>>();
        held::<(
            ZvalType,
            std::sync::Arc<[std::sync::atomic::AtomicU8]>,
            std::fs::File,
        )>();
    }

    #[test]
    fn a_cow_over_engine_state_and_a_class_cache_are_engine_state() {
        held::<std::borrow::Cow<'static, [Option<crate::ClassCache>]>>();
        held::<std::borrow::Cow<'static, str>>();
    }

    #[test]
    fn a_frozen_string_is_the_permanent_tables_and_counted_by_no_request() {
        let mut strings = Strings::new();
        let frozen = FrozenZval::interned(&mut strings, b"PHP_EOL").expect("the table has room");
        let z = frozen.zval();
        assert_eq!(z.kind(), ZvalType::String);
        assert!(!z.is_refcounted(), "an interned string is never counted");
        assert_eq!(FrozenZval::long(7).zval().kind(), ZvalType::Long);
    }

    #[test]
    fn the_permanent_table_is_engine_state_moved_and_shared() {
        held::<Strings>();
        held::<std::sync::Arc<Strings>>();
        held::<std::sync::Arc<std::sync::Mutex<Strings>>>();
    }

    /// What [`FrozenZval::of`] answers for `z`: the value it admitted.
    fn of(z: Zval) -> Option<Zval> {
        FrozenZval::of(z).map(FrozenZval::zval)
    }

    /// Constant arrays laid as the table lays one and published, past the
    /// refusal its homes make of a value no table holds (#4655): what a
    /// home without that refusal would have laid, for `of` to be shown
    /// reading it. Each is freed as this is dropped, as the table frees
    /// its own.
    #[derive(Default)]
    struct Unrefused(Vec<Arr>);

    impl Unrefused {
        fn lay(&mut self, entries: &[(Key, Zval)]) -> Zval {
            let a = crate::array::persistent_array(entries).expect("the allocator has room");
            crate::array::publish_persistent_array(a);
            self.0.push(a);
            Zval::array(a)
        }
    }

    impl Drop for Unrefused {
        fn drop(&mut self) {
            for a in self.0.drain(..) {
                crate::array::free_persistent_array(a);
            }
        }
    }

    #[test]
    fn of_admits_a_scalar_and_the_tables_strings_and_constant_arrays() {
        let _alone = crate::serial::alone();
        for z in [
            Zval::null(),
            Zval::bool(false),
            Zval::bool(true),
            Zval::long(-3),
            Zval::double(0.5),
        ] {
            assert_eq!(of(z), Some(z), "{:?} holds no pointer", z.kind());
        }
        let mut strings = Strings::new();
        let laid = strings.intern(b"laid in the table").map(Zval::string);
        let laid = laid.expect("the table has room");
        let frozen = strings.intern(b"x").map(Zval::string).expect("frozen");
        let mut pending = crate::Pending::default();
        let compiled = pending.intern(b"a cache-bound compile's", false);
        let compiled = Zval::string(compiled.expect("the allocator has room"));
        strings.publish(pending);
        for z in [laid, frozen, compiled] {
            assert_eq!(of(z), Some(z), "a string of the table");
        }
        let key = strings.intern(b"key").expect("the table has room");
        let inner = (strings.constant_array(&[(Key::Long(0), laid)], 1)).expect("a constant array");
        let entries = [
            (Key::Str(key), Zval::array(inner)),
            (Key::Long(1), compiled),
            (Key::Long(2), Zval::array(Arr::empty())),
        ];
        let outer = strings.constant_array(&entries, 3).expect("an array");
        let outer = Zval::array(outer);
        assert_eq!(of(outer), Some(outer), "a constant array of the table");
        assert_eq!(
            of(Zval::array(Arr::empty())),
            Some(Zval::array(Arr::empty()))
        );
    }

    #[test]
    fn of_refuses_a_requests_string_a_refcounted_array_and_an_object() {
        let _alone = crate::serial::alone();
        let mut heap = Heap::new();
        let string = heap.string(b"made in the request's memory");
        let string = string.expect("the heap has room");
        string.mark_valid_utf8();
        assert_eq!(of(Zval::string(string)), None, "a request's string");
        let empty = heap.array().expect("the heap has room");
        assert_eq!(of(Zval::array(empty)), None, "a refcounted array");
        let full = heap.array().expect("the heap has room");
        assert!(matches!(heap.array_append(full, Zval::long(1)), Ok(true)));
        assert_eq!(of(Zval::array(full)), None, "a refcounted array");
        let object = heap.object(1, 0, 0).expect("the heap has room");
        assert_eq!(of(Zval::object(object)), None, "an object");
        let reference = heap.reference(Zval::long(1)).expect("room");
        assert_eq!(of(Zval::reference(reference)), None, "a reference");
        let resource = heap.resource(crate::ResourceType(1)).expect("room");
        assert_eq!(of(Zval::resource(resource)), None, "a resource");
        let mut slot = Zval::long(4);
        let at = crate::ZvalRef::new(std::ptr::NonNull::from(&mut slot));
        assert_eq!(of(Zval::indirect(at)), None, "an `IS_INDIRECT`");
    }

    /// An index means something only beside the table it names, and an
    /// `UNDEF` is no value a publisher writes (#4656): each is refused
    /// as the value and as an element of a constant array, which the
    /// table refuses to lay (#4655) and `of` refuses where one stands.
    #[test]
    fn of_refuses_undef_and_a_constant_expressions_and_an_is_ptrs_index() {
        let _alone = crate::serial::alone();
        for z in [Zval::UNDEF, Zval::const_expr(2), Zval::ptr(3)] {
            assert_eq!(of(z), None, "{:?} is refused", z.kind());
        }
        let mut strings = Strings::new();
        let mut unrefused = Unrefused::default();
        for z in [Zval::UNDEF, Zval::const_expr(0), Zval::ptr(1)] {
            let entries = [(Key::Long(0), Zval::long(1)), (Key::Long(1), z)];
            let laid = strings.constant_array(&entries, 2);
            assert!(laid.is_err(), "the table refuses {:?}", z.kind());
            if z.kind() != ZvalType::Undef {
                let a = unrefused.lay(&entries);
                assert_eq!(of(a), None, "{:?} as an element", z.kind());
            }
        }
    }

    /// The bit means published (#4656): a set lays its strings and its
    /// constant arrays without it, so `of` refuses each until the set
    /// enters the table, and admits each after. The table's own constant
    /// array of a set's string is refused at its lay until then (#4655),
    /// and laid and admitted after.
    #[test]
    fn of_refuses_a_sets_string_and_constant_array_until_it_is_published() {
        let _alone = crate::serial::alone();
        let mut strings = Strings::new();
        let mut pending = crate::Pending::default();
        let s = pending.intern(b"a compile's string", false);
        let s = Zval::string(s.expect("the allocator has room"));
        let table = strings.intern(b"the table's").map(Zval::string);
        let table = table.expect("the table has room");
        let inner = pending.constant_array(&[(Key::Long(0), table)]);
        let inner = Zval::array(inner.expect("an array"));
        let key = pending.intern(b"a compile's key", false);
        let key = key.expect("the allocator has room");
        let outer = pending.constant_array(&[(Key::Str(key), inner), (Key::Long(1), s)]);
        let outer = Zval::array(outer.expect("an array"));
        let holding = strings.constant_array(&[(Key::Long(0), s)], 1);
        assert!(holding.is_err(), "the table's array of a set's string");
        assert_eq!(of(table), Some(table), "the table's own string");
        let laid = [
            (s, "a set's string"),
            (inner, "a set's constant array of the table's string"),
            (outer, "a set's constant array"),
        ];
        for (z, what) in laid {
            assert_eq!(of(z), None, "{what}, unpublished");
        }
        strings.publish(pending);
        for (z, what) in laid {
            assert_eq!(of(z), Some(z), "{what}, published");
        }
        let holding = strings.constant_array(&[(Key::Long(0), s)], 1);
        let holding = Zval::array(holding.expect("the set's string is the table's"));
        assert_eq!(of(holding), Some(holding), "the table's array of it");
    }

    /// A set's string whose bytes the table holds already is kept as a
    /// shadow, and one that takes a class cache stands in the table in
    /// place of its bytes' string: each takes the bit as the set is
    /// published, and the string it replaced keeps the one it had.
    #[test]
    fn of_admits_a_sets_shadow_and_a_class_names_string_once_published() {
        let _alone = crate::serial::alone();
        let mut strings = Strings::new();
        let held = strings.intern(b"held already").expect("the table has room");
        let named = strings.intern(b"Named").expect("the table has room");
        let mut pending = crate::Pending::default();
        let shadow = pending.intern(b"held already", false);
        let shadow = shadow.expect("the allocator has room");
        assert!(!shadow.is_same(held), "the set makes its own");
        let cache = strings.class_cache(&mut pending, b"Named");
        assert!(cache.expect("the allocator has room").is_some());
        let class = pending.intern(b"Named", false).expect("room");
        assert!(!class.is_same(named), "the set makes its own");
        for s in [shadow, class] {
            assert_eq!(of(Zval::string(s)), None, "unpublished");
        }
        strings.publish(pending);
        assert!(
            strings
                .find(b"held already")
                .is_some_and(|s| s.is_same(held))
        );
        assert!(strings.find(b"Named").is_some_and(|s| s.is_same(class)));
        for s in [shadow, class, held, named] {
            assert_eq!(of(Zval::string(s)), Some(Zval::string(s)), "published");
        }
    }

    /// The table's homes refuse to lay a constant array holding a
    /// request's value (#4655), and `of` does not rest on that: laid as
    /// the table lays one, past the refusal ([`Unrefused`]), an array one
    /// of whose keys or elements is a request's, at any depth, is
    /// refused, so `of` reads every key and element.
    #[test]
    fn of_reads_a_constant_array_through_to_each_key_and_element() {
        let _alone = crate::serial::alone();
        let mut heap = Heap::new();
        let mut unrefused = Unrefused::default();
        let string = heap.string(b"the request's").expect("room");
        let counted = heap.array().expect("the heap has room");
        let element = unrefused.lay(&[(Key::Long(0), Zval::string(string))]);
        assert_eq!(of(element), None, "a request's string as an element");
        let keyed = unrefused.lay(&[(Key::Str(string), Zval::long(1))]);
        assert_eq!(of(keyed), None, "a request's string as a key");
        let nested = unrefused.lay(&[(Key::Long(0), Zval::array(counted))]);
        assert_eq!(of(nested), None, "a refcounted array as an element");
        let deeper = unrefused.lay(&[(Key::Long(0), element)]);
        let deepest = unrefused.lay(&[(Key::Long(5), Zval::long(5)), (Key::Long(6), deeper)]);
        assert_eq!(of(deepest), None, "a request's string two arrays down");
    }

    /// An array a constant array names twice is read once: sixty-four
    /// arrays, each naming the one below it twice, are read in sixty-four
    /// steps and not in two to the sixty-fourth.
    #[test]
    fn of_reads_an_array_named_twice_once() {
        let _alone = crate::serial::alone();
        let mut strings = Strings::new();
        let mut z = Zval::string(strings.intern(b"at the bottom").expect("room"));
        for _ in 0..64 {
            let entries = [(Key::Long(0), z), (Key::Long(1), z)];
            z = Zval::array(strings.constant_array(&entries, 2).expect("an array"));
        }
        assert_eq!(of(z), Some(z));
    }
}
