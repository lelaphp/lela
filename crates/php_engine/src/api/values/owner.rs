//! One owner of a value as a type (design §5.1, #1980): [`Owner`]
//! beside the views it owns.

use std::cmp::Ordering;

use php_unit::decl::Type;

use crate::api::abi::{Callable, Class, FromArgument, Instance, IntoReturn, Raw, Static};
use crate::api::call::{Iter, Target};
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Key, PhpArray, PhpObject, PhpRef, PhpString, Resource, Value};

/// A view an [`Owner`] can own: [`Value`], the five refcounted kinds, a
/// [`Key`], the two object returns, [`Instance`] and [`Static`], a
/// [`Raw`] return, a [`Callable`], an [`Iter`], and a typed view of one
/// resource ([`ResourceView`]). Sealed but for that last door: the face
/// lists them.
pub trait View: sealed::Counted + Clone {}

/// A typed view of one [`Resource`]: the streams facet's `Stream` and
/// `StreamCtx`, or an extension's own, `Copy` and naming the resource it
/// is. An owner of one is an owner of the resource, so a constructor
/// that makes one hands back an [`Owner`] of the typed view, and a
/// native returns it as it returns the resource. The count stands on
/// the resource the owner read where it was made, whatever the view
/// answers later.
pub trait ResourceView: Copy {
    /// The resource the view is.
    fn resource(&self) -> Resource;
}

impl<T: ResourceView> sealed::Counted for T {
    type Kept = Resource;
    fn kept(&self) -> Resource {
        self.resource()
    }
    fn counted_on(&self, kept: Resource) -> Value {
        Value::resource(kept)
    }
}
impl<T: ResourceView> View for T {}
impl<T: ResourceView> sealed::One for T {}

mod sealed {
    use crate::api::values::{PhpObject, Value};

    /// What the owner's count is taken on and given up from.
    pub trait Counted {
        /// What an owner keeps beside its view to name the value its
        /// count stands on: nothing for a sealed view, which is that
        /// value, and the resource for a typed view of one, the open door,
        /// which could answer another resource later (#2209).
        type Kept: Copy + std::fmt::Debug;
        /// What an owner made of the view keeps.
        fn kept(&self) -> Self::Kept;
        /// The value an owner's count stands on, read from the view and
        /// what the owner kept.
        fn counted_on(&self, kept: Self::Kept) -> Value;
        /// The value the view names, no owner taken.
        fn counted(&self) -> Value {
            self.counted_on(self.kept())
        }

        /// The object a callable resolved to where its value does not
        /// name it, which an owner of the callable counts beside the
        /// value ([`super::Callable`]), read from the view; no other view
        /// has one.
        fn receiver(&self) -> Option<PhpObject> {
            None
        }

        /// The view moved out of an owner that is forgotten next: a copy,
        /// for a view with no drop glue, which is every view but
        /// [`super::Callable`].
        fn moved_out(&mut self) -> Self
        where
            Self: Clone,
        {
            self.clone()
        }
    }

    /// A view whose owner is the one count on [`Counted::counted`], so
    /// it is recast to an owner of another view of that value. Every
    /// view but [`super::Callable`].
    pub trait One: Counted {}
}

macro_rules! view {
    ([$($generics:tt)*] $t:ty, $make:expr) => {
        impl<$($generics)*> sealed::Counted for $t {
            type Kept = ();
            fn kept(&self) {}
            fn counted_on(&self, (): ()) -> Value {
                ($make)(self.clone())
            }
        }
        impl<$($generics)*> View for $t {}
        impl<$($generics)*> sealed::One for $t {}
    };
    ($t:ty, $make:expr) => {
        view!([] $t, $make);
    };
}
view!(Value, |v: Value| v);
view!(PhpString, Value::string);
view!(PhpArray, Value::array);
view!(PhpObject, Value::object);
view!(PhpRef, Value::reference);
view!(Resource, Value::resource);

view!(Key, |k: Key| match k {
    Key::Int(n) => Value::int(n),
    Key::Str(s) => Value::string(s),
});
view!(Static, |s: Static| Value::object(s.0));
view!(Iter, |i: Iter| i.as_value());

view!([C: Class] Instance<C>, |i: Instance<C>| Value::object(i.object()));
view!([T: FromArgument] Raw<T>, |r: Raw<T>| r.value());

/// A callable kept past the frame it was read off, `zend_fcc_dup`: an
/// owner of it is the claim `zend_fcc_addref` (`Z/Zend/zend_API.h:773`)
/// makes, counted on its value and, `GC_ADDREF(fcc->object)`, on the
/// receiver it resolved to where that is not the value. A
/// `Class::method` string taken with `$this` in scope names an object
/// the string does not own, so the owner owns it; a closure's count is
/// its value's, since `fcc->closure` is the value. [`Owner::give_up`]
/// is `zend_fcc_dtor`, the receiver given up first. The owner keeps
/// nothing beside the view: the receiver is read from it. The owner is
/// never recast to an owner of one value: it is two counts.
impl sealed::Counted for Callable {
    type Kept = ();
    fn kept(&self) {}
    fn counted_on(&self, (): ()) -> Value {
        self.value()
    }

    fn moved_out(&mut self) -> Callable {
        Callable::moved_out(self)
    }

    fn receiver(&self) -> Option<PhpObject> {
        match self.target() {
            Target::Method {
                this: Some(this), ..
            } => Some(this),
            Target::Function(_) | Target::Method { this: None, .. } | Target::Closure(_) => None,
        }
    }
}
impl View for Callable {}

/// One owner of the value a view names: one counted claim on it, the
/// glossary's **Owner**, where the view itself carries none. Neither
/// `Copy` nor `Clone`, and must-use, so one taken is one handed on: to a
/// sink, which takes it from its caller, to the engine as a native's
/// return, or given up with [`Owner::give_up`]. It dereferences to the
/// view it owns, and a view copied out of it is a view: copying never
/// moves the owner.
///
/// [`Owner::take`] is the one way a native comes by one, `Z_TRY_ADDREF`
/// over a view: nothing for a scalar, an interned string or an
/// immutable array, which are owned by no count, and an owner of them
/// all the same, so the code that holds one is the code that would hold
/// a counted value. Only the engine names an owner it did not take, the
/// one a value is made with.
///
/// A debug build holds the type to its exits: an owner dropped
/// unconsumed releases nothing, as no `Drop` here ever does, and is
/// recorded on its request's heap, naming the line that made it, and
/// raised as the `InternalError` that ends the request at the engine's
/// next sensing point. The line is kept beside the block the owner
/// stands on, in its heap's table ([`php_heap::owners`]), not in the
/// owner, so an owner is its view in every build and a payload held to
/// php's size holds one as php holds the `zval` (#2246); an owner of
/// what no count owns, which leaks nothing, is recorded nowhere. A
/// release build gives it no `Drop`, so no drop glue runs (design §5.1),
/// and what such an owner kept is freed with the request's memory at
/// its end.
///
/// A copied owner is a moved one, used once:
///
/// ```compile_fail,E0382
/// # use php_engine::api::request::Request;
/// # use php_engine::api::values::{Owner, Value};
/// fn twice(req: &mut Request, value: Value) {
///     let owner = Owner::take(req, value);
///     let copy = owner;
///     owner.give_up(req);
///     copy.give_up(req);
/// }
/// # fn main() {}
/// ```
///
/// A cloned owner does not exist; what `clone` reaches through the
/// dereference is the view:
///
/// ```compile_fail,E0308
/// # use php_engine::api::request::Request;
/// # use php_engine::api::values::{Owner, Value};
/// fn cloned(req: &mut Request, value: Value) -> Owner<Value> {
///     let owner = Owner::take(req, value);
///     let second: Owner<Value> = owner.clone();
///     owner.give_up(req);
///     second
/// }
/// # fn main() {}
/// ```
///
/// ```compile_fail,E0277
/// # use php_engine::api::values::{Owner, Value};
/// fn cloned<T: Clone>(owner: &T) -> T { owner.clone() }
/// fn twice(owner: &Owner<Value>) -> Owner<Value> { cloned(owner) }
/// # fn main() {}
/// ```
///
/// An extension cannot name an owner it did not take, neither through
/// the engine's own constructor nor by building one, which rustc refuses
/// with no code:
///
/// ```compile_fail,E0624
/// # use php_engine::api::request::Request;
/// # use php_engine::api::values::{Owner, Value};
/// fn fabricated(req: &mut Request, value: Value) -> Owner<Value> {
///     Owner::adopt(value)
/// }
/// # fn main() {}
/// ```
///
/// ```compile_fail
/// # use php_engine::api::values::{Owner, Value};
/// fn fabricated(value: Value) -> Owner<Value> {
///     Owner { view: value }
/// }
/// # fn main() {}
/// ```
#[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
#[derive(Debug)]
pub struct Owner<T: View> {
    view: T,
    /// What names the value the count stands on beside the view, read
    /// where the owner was made: nothing for a sealed view, so an owner of
    /// one is the view alone.
    kept: T::Kept,
}

impl<T: View> Owner<T> {
    /// One more owner of what `view` names, `Z_TRY_ADDREF`: the one
    /// conversion from a view to an owner. What a native does where php
    /// copies a value it holds into a place of its own, `ZVAL_COPY` and
    /// `zend_string_copy`, and where it returns a view it is about to
    /// let go of. A view returned as it stands needs none: the engine
    /// takes its own owner of it, `RETURN_COPY` ([`IntoReturn`]).
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn take(req: &Request, view: T) -> Owner<T> {
        let kept = view.kept();
        req.addref(&view.counted_on(kept));
        if let Some(receiver) = view.receiver() {
            req.addref(&receiver.as_value());
        }
        Owner::of(view, kept)
    }

    /// The owner `view` already is: the count a value is made with,
    /// which only the engine hands out.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn adopt(view: T) -> Owner<T> {
        let kept = view.kept();
        Owner::of(view, kept)
    }

    /// The owner of what `view` and `kept` name, its line kept in a
    /// debug build's table.
    #[cfg_attr(debug_assertions, track_caller)]
    fn of(view: T, kept: T::Kept) -> Owner<T> {
        let owner = Owner { view, kept };
        #[cfg(debug_assertions)]
        php_heap::owners::made(owner.sensed_on(), std::panic::Location::caller());
        owner
    }

    /// The owner given up, `zval_ptr_dtor`: the value is freed with its
    /// last owner, an object's destructor served first, and is a possible
    /// root of the cycle collector where owners are left (design §5.12).
    // The caller's line is what a debug build's freed-use guard names
    // where the release is refused.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn give_up(self, req: &mut Request) {
        let (view, kept) = self.consume();
        if let Some(receiver) = view.receiver() {
            req.release(receiver.as_value());
        }
        req.release(view.counted_on(kept));
    }

    /// The owner given up as `zval_ptr_dtor_nogc` gives one up: the value
    /// destroyed at its last owner, and where owners are left no possible
    /// root of the cycle collector (design §5.12). What a walk gives up of
    /// what it held where php's held nothing, `zend_array_release`, and
    /// what a native gives up where php's gives up its `ZVAL_COPY` with
    /// `zval_ptr_dtor_nogc` (`json_encode`).
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn give_up_nogc(self, req: &mut Request) {
        let (view, kept) = self.consume();
        if let Some(receiver) = view.receiver() {
            req.release_nogc(receiver.as_value());
        }
        req.release_nogc(view.counted_on(kept));
    }

    /// The owner left to the request's end, as php's fatal leaves every
    /// owner it held: a fatal is ending the request, no code runs after
    /// it, and the heap is freed whole with the request (design §5.1).
    /// The fatal it is left to is its argument, so only a path that hands
    /// one on can take it.
    pub(crate) fn abandon(self, _ending: &php_heap::Fatal) {
        self.consume();
    }

    /// The view, the owner handed on with it: what a sink keeps, and what
    /// the engine is handed as a native's return.
    pub(crate) fn hand_on(self) -> T {
        self.consume().0
    }

    /// The value the count stands on.
    fn stands_on(&self) -> Value {
        self.view.counted_on(self.kept)
    }

    /// The value a debug build keeps the owner's line beside: the one
    /// its count stands on, or where that is counted by nothing, the
    /// receiver a callable's owner counts beside it.
    #[cfg(debug_assertions)]
    fn sensed_on(&self) -> php_heap::Zval {
        let value = self.stands_on();
        match self.view.receiver() {
            Some(receiver) if !value.is_refcounted() => receiver.as_value(),
            _ => value,
        }
        .to_zval()
    }

    /// The owner consumed, its line taken off a debug build's table
    /// before its count moves, and what it was made of moved out without
    /// its `Drop`, which records only an owner never consumed.
    fn consume(self) -> (T, T::Kept) {
        #[cfg(debug_assertions)]
        php_heap::owners::consumed(self.sensed_on());
        self.disarmed()
    }

    /// What the owner was made of, moved out, and the owner forgotten:
    /// the view's own drop glue, a callable's name, moved out with it, so
    /// the forgotten owner holds none.
    #[expect(
        clippy::mem_forget,
        reason = "a consumed owner skips the debug sensor's Drop, and in either build the view left behind holds nothing to drop"
    )]
    fn disarmed(mut self) -> (T, T::Kept) {
        let view = sealed::Counted::moved_out(&mut self.view);
        let kept = self.kept;
        std::mem::forget(self);
        (view, kept)
    }
}

/// What an owner of one count does with it: the count let go as a
/// handler lets one go, and the owner recast to another view of the
/// value. An owner of a [`Callable`] is two counts and does neither.
impl<T: View + sealed::One> Owner<T> {
    /// The owner given up as `GC_DELREF` gives one up, and what is left
    /// of the value: at the last owner the value is destroyed,
    /// `zend_array_destroy`, `zend_string_efree` or
    /// `zend_objects_store_del` with its destructor served, and where
    /// owners are left nothing else happens, no possible root of the
    /// cycle collector (design §5.12). What php's handlers do after a
    /// diagnostic, and the test they make after it: whether the call the
    /// owner was held across freed the value or took an owner of its own.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn let_go(self, req: &mut Request) -> Left {
        let (view, kept) = self.consume();
        let value = view.counted_on(kept);
        if !value.is_refcounted() {
            return Left::Uncounted;
        }
        let owners = value.refcount().saturating_sub(1);
        req.release_nogc(value);
        Left::Owners(owners)
    }

    /// The owner of the value as a [`Value`], the same count: what a
    /// native returns as `mixed` or hands to a sink that takes a value.
    #[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
    pub fn into_value(self) -> Owner<Value> {
        let value = self.stands_on();
        self.recast(value, ())
    }

    /// The same owner over another view of its value, `kept` naming the
    /// same value, so a debug build's line beside the block is its, and
    /// this one is consumed.
    fn recast<U: View + sealed::One>(self, view: U, kept: U::Kept) -> Owner<U> {
        self.disarmed();
        Owner { view, kept }
    }
}

impl<T: ResourceView> Owner<T> {
    /// The owner of the typed view as the [`Resource`] it is, the same
    /// count: what a native returns a stream it made as.
    #[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
    pub fn into_resource(self) -> Owner<Resource> {
        let resource = self.kept;
        self.recast(resource, ())
    }
}

impl Owner<PhpString> {
    /// The owner of the string as the string key it is, the same count,
    /// not read as a number: what `zend_hash_update` is handed, where
    /// [`Key::from_string`] is `zend_symtable_update`'s key.
    #[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
    pub fn into_key(self) -> Owner<Key> {
        let key = Key::Str(self.view);
        self.recast(key, ())
    }
}

impl Owner<Resource> {
    /// The owner of the resource as the typed view `typed` makes of it,
    /// the same count: what a facet hands back of a resource it has just
    /// made, the owner `Resource::sized` gave it as a stream's. `typed`
    /// vouches that the view is the resource it is handed, as
    /// [`Owner::into_instance`]'s body vouches for its class; a debug
    /// build holds it to that.
    #[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
    pub fn into_typed<T: ResourceView>(self, typed: fn(Resource) -> T) -> Owner<T> {
        let view = typed(self.view);
        debug_assert!(
            view.resource() == self.view,
            "a typed view of another resource"
        );
        let kept = self.view;
        self.recast(view, kept)
    }
}

/// An owner of a value that carries no count, made without the request:
/// `ZVAL_LONG`, `ZVAL_BOOL`, `ZVAL_DOUBLE` and `ZVAL_NULL` into a place
/// of the native's own, where [`Owner::take`] would take no count either.
/// One constructor per kind, each over the scalar alone, so none can name
/// a counted value, and a debug build's owner records no line: dropped
/// unconsumed, it leaks nothing, so its heap records nothing.
///
/// ```compile_fail,E0308
/// # use php_engine::api::values::{Owner, PhpString, Value};
/// fn counted(s: PhpString) -> Owner<Value> {
///     Owner::int(s)
/// }
/// # fn main() {}
/// ```
///
/// ```compile_fail,E0599
/// # use php_engine::api::values::{Owner, PhpString};
/// fn counted() -> Owner<PhpString> {
///     Owner::<PhpString>::null()
/// }
/// # fn main() {}
/// ```
impl Owner<Value> {
    /// The owner of `null`.
    pub const fn null() -> Owner<Value> {
        Owner::scalar(Value::null())
    }

    /// The owner of `b`.
    pub const fn bool(b: bool) -> Owner<Value> {
        Owner::scalar(Value::bool(b))
    }

    /// The owner of `n`.
    pub const fn int(n: i64) -> Owner<Value> {
        Owner::scalar(Value::int(n))
    }

    /// The owner of `f`.
    pub const fn float(f: f64) -> Owner<Value> {
        Owner::scalar(Value::float(f))
    }

    /// The owner of a value its constructor vouches carries no count.
    const fn scalar(view: Value) -> Owner<Value> {
        Owner { view, kept: () }
    }

    /// The owner of the object the value is, the same count: what a
    /// native keeps of a call's answer it holds as an object
    /// (`getIterator()`, a factory's), where php keeps the `zval` it was
    /// handed. The owner is handed back as it is where the value is no
    /// object.
    pub fn into_object(self) -> Result<Owner<PhpObject>, Owner<Value>> {
        match self.view.as_object() {
            Some(object) => Ok(self.recast(object, ())),
            None => Err(self),
        }
    }

    /// The owner of the value returned as it is under `T`'s declaration,
    /// the same count ([`Raw`]).
    #[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
    pub fn into_raw<T: FromArgument>(self) -> Owner<Raw<T>> {
        let raw = Raw::at(self.view, 0);
        self.recast(raw, ())
    }

    /// `ZVAL_DEREF` and then `convert_to_*` (`Z/Zend/zend_operators.c`):
    /// the value this owner holds replaced by `value`, its conversion,
    /// through the reference where the owner holds one
    /// ([`PhpRef::convert`]), so a variable bound by reference reads the
    /// conversion, and in place otherwise. What the slot held is given up
    /// after the write. What binding a parameter of PDO's or `SQLite3`'s
    /// does to it.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $s = $db->prepare('SELECT ?, ?');
    /// $n = '42'; $f = '1.5';
    /// $s->bindParam(1, $n, SQLITE3_INTEGER);
    /// $s->bindParam(2, $f, SQLITE3_FLOAT);
    /// $s->execute();
    /// var_dump($n, $f);
    /// $pdo = new PDO('sqlite::memory:');
    /// $q = $pdo->prepare('SELECT ?, ?');
    /// $i = '7'; $t = 2.5;
    /// $q->bindParam(1, $i, PDO::PARAM_INT);
    /// $q->bindParam(2, $t, PDO::PARAM_STR);
    /// $q->execute();
    /// var_dump($i, $t);
    /// $b = true;
    /// $q->bindParam(1, $b, PDO::PARAM_INT);
    /// var_dump($b);
    /// ```
    /// ```expect
    /// int(42)
    /// float(1.5)
    /// int(7)
    /// string(3) "2.5"
    /// int(1)
    /// ```
    pub fn convert(&mut self, req: &mut Request, value: Owner<Value>) {
        match self.view.as_php_ref() {
            Some(cell) => cell.convert(req, value),
            None => std::mem::replace(self, value).give_up(req),
        }
    }
}

/// The owner of an integer key, made without the request, as
/// [`Owner::<Value>::int`](Owner::int) is of an integer: `zend_hash_index_update`'s `h`,
/// which no count owns, so a debug build's owner records no line. The
/// constructor takes the integer alone, so it cannot name a string key,
/// whose owner [`Owner::take`] or [`Owner::into_key`] makes.
///
/// ```compile_fail,E0308
/// # use php_engine::api::values::{Key, Owner, PhpString};
/// fn counted(s: PhpString) -> Owner<Key> {
///     Owner::index(s)
/// }
/// # fn main() {}
/// ```
impl Owner<Key> {
    /// The owner of the integer key `n`.
    pub const fn index(n: i64) -> Owner<Key> {
        Owner {
            view: Key::Int(n),
            kept: (),
        }
    }
}

impl Owner<Callable> {
    /// `zend_get_gc_buffer_add_fcc`: each value the owner counts, the
    /// callable's value and the receiver it resolved to, handed to a
    /// `get_gc`'s `visit`, so the cycle collector reads the owner's
    /// counts as the edges they are.
    pub fn visit(&self, visit: &mut dyn FnMut(Value)) {
        visit(self.stands_on());
        if let Some(receiver) = sealed::Counted::receiver(&self.view) {
            visit(receiver.as_value());
        }
    }
}

impl Owner<PhpObject> {
    /// The owner of the object as the `C` the body vouches it is, the
    /// same count, to return ([`Instance::of`]).
    #[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
    pub fn into_instance<C: Class>(self) -> Owner<Instance<C>> {
        let instance = Instance::of(self.view);
        self.recast(instance, ())
    }

    /// The owner of the object as `static`, the same count, to return.
    #[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
    pub fn into_static(self) -> Owner<Static> {
        let object = Static(self.view);
        self.recast(object, ())
    }
}

/// The writes of an owned array, [`PhpArray`]'s own, through the owner:
/// a write that separates moves the owner to the copy, as it moves the
/// name, and the owner lends no `&mut PhpArray`, through which a view
/// could be put where the owner stands.
impl Owner<PhpArray> {
    /// A write through the owner's array. The count stands on the array
    /// the owner names, so where the write separated it the count moved
    /// with the name.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(super) fn written<R>(&mut self, write: impl FnOnce(&mut PhpArray) -> R) -> R {
        #[cfg(debug_assertions)]
        let made = php_heap::owners::consumed(self.sensed_on());
        let written = write(&mut self.view);
        #[cfg(debug_assertions)]
        php_heap::owners::made(
            self.sensed_on(),
            made.unwrap_or_else(std::panic::Location::caller),
        );
        written
    }

    /// Remove `key` from this very table, whatever its other owners,
    /// `zend_hash_del` under `HT_ALLOW_COW_VIOLATION`: `true` when it was
    /// there, its value released. What `php_splice` does to the table it
    /// holds an owner of across its removal (`GC_ADDREF(in_hash)`,
    /// `Z/ext/standard/array.c:3321`), so a holder that took the table
    /// while a destructor of the removal ran sees the removal rather than
    /// a copy separated from under the name that holds it. This owner is
    /// that hold, kept for as long as the rewrite deletes; an immutable
    /// table, which no request may write, is left as it is.
    pub fn unset_in_place(&self, req: &mut Request, key: &Key) -> bool {
        self.view.del_in_place(req, key)
    }

    /// [`PhpArray::set`].
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn set(
        &mut self,
        req: &mut Request,
        key: Owner<Key>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        self.written(|view| view.set(req, key, value))
    }

    /// [`PhpArray::set_str`].
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn set_str(
        &mut self,
        req: &mut Request,
        name: &[u8],
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        self.written(|view| view.set_str(req, name, value))
    }

    /// [`PhpArray::set_symtable_str`].
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn set_symtable_str(
        &mut self,
        req: &mut Request,
        name: &[u8],
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        self.written(|view| view.set_symtable_str(req, name, value))
    }

    /// [`PhpArray::add_new`].
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn add_new(
        &mut self,
        req: &mut Request,
        key: Owner<Key>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        self.written(|view| view.add_new(req, key, value))
    }

    /// [`PhpArray::replace`].
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn replace(
        &mut self,
        req: &mut Request,
        key: &Key,
        value: Owner<Value>,
    ) -> Result<bool, Thrown> {
        self.written(|view| view.replace(req, key, value))
    }

    /// [`PhpArray::append`].
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn append(&mut self, req: &mut Request, value: Owner<Value>) -> Result<(), Thrown> {
        self.written(|view| view.append(req, value))
    }

    /// [`PhpArray::try_append`].
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn try_append(&mut self, req: &mut Request, value: Owner<Value>) -> Result<bool, Thrown> {
        self.written(|view| view.try_append(req, value))
    }

    /// [`PhpArray::unset`].
    pub fn unset(&mut self, req: &mut Request, key: &Key) -> Result<bool, Thrown> {
        self.written(|view| view.unset(req, key))
    }

    /// [`PhpArray::set_next_free`].
    pub fn set_next_free(&mut self, req: &mut Request, next: i64) -> Result<(), Thrown> {
        self.written(|view| view.set_next_free(req, next))
    }

    /// [`PhpArray::separate`].
    pub fn separate(&mut self, req: &mut Request) -> Result<(), Thrown> {
        self.written(|view| view.separate(req))
    }

    /// [`PhpArray::sort_by`].
    pub fn sort_by(
        &mut self,
        req: &mut Request,
        renumber: bool,
        compare: impl FnMut(&mut Request, (&Key, &Value), (&Key, &Value)) -> Result<Ordering, Thrown>,
    ) -> Result<(), Thrown> {
        self.written(|view| view.sort_by(req, renumber, compare))
    }

    /// [`PhpArray::cursor_next`].
    pub fn cursor_next(&mut self) {
        self.view.cursor_next();
    }

    /// [`PhpArray::cursor_prev`].
    pub fn cursor_prev(&mut self) {
        self.view.cursor_prev();
    }

    /// [`PhpArray::cursor_reset`].
    pub fn cursor_reset(&mut self) {
        self.view.cursor_reset();
    }

    /// [`PhpArray::cursor_end`].
    pub fn cursor_end(&mut self) {
        self.view.cursor_end();
    }
}

/// What the engine made, as the owner it is: the face's constructors
/// over the engine's own, whose view is the one owner the value was made
/// with ([`Owner::adopt`]).
#[cfg_attr(debug_assertions, track_caller)]
pub(crate) fn adopted<T: View>(made: Result<T, Thrown>) -> Result<Owner<T>, Thrown> {
    match made {
        Ok(view) => Ok(Owner::adopt(view)),
        Err(thrown) => Err(thrown),
    }
}

/// [`adopted`] over what may be nothing.
#[cfg_attr(debug_assertions, track_caller)]
pub(crate) fn adopted_some<T: View>(
    made: Result<Option<T>, Thrown>,
) -> Result<Option<Owner<T>>, Thrown> {
    match made {
        Ok(Some(view)) => Ok(Some(Owner::adopt(view))),
        Ok(None) => Ok(None),
        Err(thrown) => Err(thrown),
    }
}

/// What is left of a value once an owner of it is let go
/// ([`Owner::let_go`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Left {
    /// Nothing was counted: a scalar, an interned string or an immutable
    /// array, which no handler frees (php's `IS_ARRAY_IMMUTABLE` test
    /// before its `GC_ADDREF`).
    Uncounted,
    /// The owners besides the one let go, as they stand after it; none
    /// is the value freed as it was let go.
    Owners(u32),
}

impl Left {
    /// The owner let go was the last, and the value is gone:
    /// `GC_DELREF(ht) == 0`.
    pub(crate) fn freed(self) -> bool {
        self == Left::Owners(0)
    }

    /// Anything but the one owner the write started from is left, so
    /// the write lands nowhere: `GC_DELREF(ht) != 1` in
    /// `slow_index_convert_w` and `zend_undefined_index_write`.
    pub(crate) fn not_alone(self) -> bool {
        matches!(self, Left::Owners(owners) if owners != 1)
    }
}

// An owner of a value is the value (#2209), in every build (#2246): what
// it keeps beside a sealed view is nothing, and a debug build's line is
// kept beside the block it stands on. Held at every build, so no lane can
// skip it.
const _: () = {
    assert!(size_of::<Owner<Value>>() == size_of::<Value>());
    assert!(size_of::<Owner<PhpString>>() == size_of::<usize>());
    assert!(size_of::<Owner<PhpArray>>() == size_of::<usize>());
    assert!(size_of::<Owner<PhpObject>>() == size_of::<usize>());
    assert!(size_of::<Owner<Resource>>() == size_of::<usize>());
};

impl<T: View> std::ops::Deref for Owner<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.view
    }
}

/// A debug build's sensor: an owner dropped unconsumed releases nothing
/// and is recorded on its heap with the line that made it. A consumed
/// owner is forgotten, so this runs only for one never consumed.
#[cfg(debug_assertions)]
impl<T: View> Drop for Owner<T> {
    fn drop(&mut self) {
        php_heap::owners::dropped(self.sensed_on());
    }
}

/// An owner returned from a native is moved to the engine: the caller's
/// temporary holds it, and nothing more is taken. The value is the one
/// the count stands on, and not `T::into_return`'s, which is a view's
/// return.
impl<T: View + sealed::One + IntoReturn> IntoReturn for Owner<T> {
    const UNDECLARED: bool = T::UNDECLARED;
    const MASK: u32 = T::MASK;
    fn declared() -> Option<Type> {
        T::declared()
    }
    fn into_return(self, _: &mut Request) -> Result<Owner<Value>, Thrown> {
        Ok(self.into_value())
    }
}
