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
//! state, a `Cow` among them, an owned fd, a file and the standard
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

use crate::{Bytes, ClassCache, Fatal, Intern, Strings, Zval, ZvalType};

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

/// The engine's permanent table (`string.rs`, #4633's ruling): what it
/// holds is its own, laid off every request's heap, and immutable once
/// it is published. A string of it is "immutable, hashed before it is
/// published, never refcounted and never freed while the engine
/// stands", laid in its bump or by a set a cache-bound compile filled
/// on the system allocator ([`crate::Pending`]); a constant array of it
/// is one `persistent_array` laid on the system allocator, marked
/// `IS_ARRAY_IMMUTABLE`, and freed with the table. The pointers it holds
/// are to those blocks, its bump's chunks and the class caches' numbers,
/// and to nothing of a request's. Its `Send` and `Sync` impls make the
/// same argument for moving and sharing it: the only other names for
/// its strings and arrays are the zvals of the units the engine
/// published, which read and never write them, and growing it takes
/// `&mut self`.
impl EngineState for Strings {}

/// A value no request owns: a scalar, or a string of the engine's
/// permanent table, [`Strings`]. It is how engine state holds a value
/// where a [`Zval`] could be a request's: `Engine::constant_values` is
/// one per constant, made where the engine publishes the constant at
/// `MINIT`, and a request reads it as the [`Zval`] it is. Made from
/// nothing else, so no request's value can be one.
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
    /// the first ask, and the fatal the table raised where it had no room.
    pub fn interned(strings: &mut Strings, bytes: &[u8]) -> Result<FrozenZval, Fatal> {
        strings.intern(bytes).map(|s| FrozenZval(Zval::string(s)))
    }

    /// The value, for a request to read: a scalar, or an interned string
    /// no request counts.
    #[must_use]
    pub const fn zval(self) -> Zval {
        self.0
    }
}

// A frozen value is a scalar or a string of the permanent table, which
// is immutable, never refcounted and alive while the engine stands
// (`Strings`); its constructors make nothing else.
impl EngineState for FrozenZval {}

#[cfg(test)]
mod tests {
    use super::{EngineState, FrozenZval};
    use crate::{Strings, ZvalType};

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
}
