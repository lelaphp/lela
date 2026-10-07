//! One owner of a value as a type (design §5.1, #1980): [`Owner`]
//! beside the views it owns.

use std::cmp::Ordering;

use php_unit::decl::Type;

use crate::api::abi::{Class, FromArgument, Instance, IntoReturn, Raw, Static};
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Key, PhpArray, PhpObject, PhpRef, PhpString, Resource, Value};

/// A view an [`Owner`] can own: [`Value`], the five refcounted kinds, a
/// [`Key`], the two object returns, [`Instance`] and [`Static`], a
/// [`Raw`] return, and a typed view of one resource ([`ResourceView`]).
/// Sealed but for that last door: the face lists them.
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

impl ResourceView for Resource {
    fn resource(&self) -> Resource {
        *self
    }
}

impl<T: ResourceView> sealed::Counted for T {
    fn counted(&self) -> Value {
        Value::resource(self.resource())
    }
}
impl<T: ResourceView> View for T {}

mod sealed {
    use crate::api::values::Value;

    /// What the owner's count is taken on and given up from.
    pub trait Counted {
        /// The value the view names, no owner taken.
        fn counted(&self) -> Value;
    }
}

macro_rules! view {
    ($t:ty, $make:expr) => {
        impl sealed::Counted for $t {
            fn counted(&self) -> Value {
                ($make)(self.clone())
            }
        }
        impl View for $t {}
    };
}
view!(Value, |v: Value| v);
view!(PhpString, Value::string);
view!(PhpArray, Value::array);
view!(PhpObject, Value::object);
view!(PhpRef, Value::reference);

view!(Key, |k: Key| match k {
    Key::Int(n) => Value::int(n),
    Key::Str(s) => Value::string(s),
});
view!(Static, |s: Static| Value::object(s.0));

impl<C: Class> sealed::Counted for Instance<C> {
    fn counted(&self) -> Value {
        Value::object(self.object())
    }
}
impl<C: Class> View for Instance<C> {}

impl<T: FromArgument> sealed::Counted for Raw<T> {
    fn counted(&self) -> Value {
        self.value()
    }
}
impl<T: FromArgument> View for Raw<T> {}

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
/// next sensing point. A release build gives it no `Drop`, so no drop
/// glue runs (design §5.1), and what such an owner kept is freed with
/// the request's memory at its end.
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
///     Owner::adopt(req, value)
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
    /// The value the count stands on, read where the owner was made.
    counted: Value,
    /// A debug build's record of where the owner was made, until it is
    /// consumed.
    #[cfg(debug_assertions)]
    made: Option<Made>,
}

/// Where a debug build's owner was made, and its way to the heap's
/// sensed record ([`php_heap::Sensor`]).
#[cfg(debug_assertions)]
#[derive(Debug)]
struct Made {
    site: &'static std::panic::Location<'static>,
    sensor: php_heap::Sensor,
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
        let counted = view.counted();
        req.addref(&counted);
        Owner::of(req, view, counted)
    }

    /// The owner `view` already is: the count a value is made with,
    /// which only the engine hands out.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn adopt(req: &Request, view: T) -> Owner<T> {
        let counted = view.counted();
        Owner::of(req, view, counted)
    }

    /// The owner of `counted`, presented as `view`.
    #[cfg_attr(debug_assertions, track_caller)]
    fn of(req: &Request, view: T, counted: Value) -> Owner<T> {
        #[cfg(not(debug_assertions))]
        let _ = req;
        Owner {
            view,
            counted,
            #[cfg(debug_assertions)]
            made: Some(Made {
                site: std::panic::Location::caller(),
                sensor: req.sensor(),
            }),
        }
    }

    /// The owner given up, `zval_ptr_dtor`: the value is freed with its
    /// last owner, an object's destructor served first, and is a possible
    /// root of the cycle collector where owners are left (design §5.12).
    // Taking the owner is the point: one given up cannot be given up
    // again.
    // The caller's line is what a debug build's freed-use guard names
    // where the release is refused.
    #[allow(clippy::needless_pass_by_value)]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn give_up(mut self, req: &mut Request) {
        self.consume();
        req.release(self.counted);
    }

    /// The owner given up as `zval_ptr_dtor_nogc` gives one up: the value
    /// destroyed at its last owner, and where owners are left no possible
    /// root of the cycle collector (design §5.12). What a walk gives up of
    /// what it held where php's held nothing, and `zend_array_release`.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn give_up_nogc(mut self, req: &mut Request) {
        self.consume();
        req.release_nogc(self.counted);
    }

    /// The owner given up as `GC_DELREF` gives one up, and what is left
    /// of the value: at the last owner the value is destroyed,
    /// `zend_array_destroy`, `zend_string_efree` or
    /// `zend_objects_store_del` with its destructor served, and where
    /// owners are left nothing else happens, no possible root of the
    /// cycle collector (design §5.12). What php's handlers do after a
    /// diagnostic, and the test they make after it: whether the call the
    /// owner was held across freed the value or took an owner of its own.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn let_go(mut self, req: &mut Request) -> Left {
        self.consume();
        let value = self.counted;
        if !value.is_refcounted() {
            return Left::Uncounted;
        }
        let owners = value.refcount().saturating_sub(1);
        req.release_nogc(value);
        Left::Owners(owners)
    }

    /// The owner left to the request's end, as php's fatal leaves every
    /// owner it held: a fatal is ending the request, no code runs after
    /// it, and the heap is freed whole with the request (design §5.1).
    /// The fatal it is left to is its argument, so only a path that hands
    /// one on can take it.
    pub(crate) fn abandon(mut self, _ending: &php_heap::Fatal) {
        self.consume();
    }

    /// The view, the owner handed on with it: what a sink keeps, and what
    /// the engine is handed as a native's return.
    // Taking the owner is the point: the sink holds it from here.
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn hand_on(mut self) -> T {
        self.consume();
        self.view.clone()
    }

    /// The owner of the value as a [`Value`], the same count: what a
    /// native returns as `mixed` or hands to a sink that takes a value.
    #[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
    pub fn into_value(self) -> Owner<Value> {
        let value = self.counted;
        self.recast(value)
    }

    /// The same owner over another view of its value.
    fn recast<U: View>(mut self, view: U) -> Owner<U> {
        Owner {
            view,
            counted: self.counted,
            #[cfg(debug_assertions)]
            made: self.made.take(),
        }
    }

    /// The owner consumed, so a debug build's `Drop` records nothing.
    // A release build's owner has nothing to mark.
    #[allow(clippy::unused_self)]
    fn consume(&mut self) {
        #[cfg(debug_assertions)]
        {
            self.made = None;
        }
    }
}

impl<T: ResourceView> Owner<T> {
    /// The owner of the typed view as the [`Resource`] it is, the same
    /// count: what a native returns a stream it made as.
    #[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
    pub fn into_resource(self) -> Owner<Resource> {
        let resource = self.view.resource();
        self.recast(resource)
    }
}

impl Owner<PhpString> {
    /// The owner of the string as the string key it is, the same count,
    /// not read as a number: what `zend_hash_update` is handed, where
    /// [`Key::from_string`] is `zend_symtable_update`'s key.
    #[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
    pub fn into_key(self) -> Owner<Key> {
        let key = Key::Str(self.view);
        self.recast(key)
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
        self.recast(view)
    }
}

impl Owner<Value> {
    /// The owner of the value returned as it is under `T`'s declaration,
    /// the same count ([`Raw`]).
    #[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
    pub fn into_raw<T: FromArgument>(self) -> Owner<Raw<T>> {
        let raw = Raw::returned(self.view);
        self.recast(raw)
    }
}

impl Owner<PhpObject> {
    /// The owner of the object as the `C` the body vouches it is, the
    /// same count, to return ([`Instance::of`]).
    #[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
    pub fn into_instance<C: Class>(self) -> Owner<Instance<C>> {
        let instance = Instance::of(self.view);
        self.recast(instance)
    }

    /// The owner of the object as `static`, the same count, to return.
    #[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
    pub fn into_static(self) -> Owner<Static> {
        let object = Static(self.view);
        self.recast(object)
    }
}

/// The writes of an owned array, [`PhpArray`]'s own, through the owner:
/// a write that separates moves the owner to the copy, as it moves the
/// name, and the owner lends no `&mut PhpArray`, through which a view
/// could be put where the owner stands.
impl Owner<PhpArray> {
    /// A write through the owner's array, its count moved with the name
    /// where the write separated the array.
    pub(super) fn written<R>(&mut self, write: impl FnOnce(&mut PhpArray) -> R) -> R {
        let answer = write(&mut self.view);
        self.counted = Value::array(self.view);
        answer
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
pub(crate) fn adopted<T: View>(req: &Request, made: Result<T, Thrown>) -> Result<Owner<T>, Thrown> {
    match made {
        Ok(view) => Ok(Owner::adopt(req, view)),
        Err(thrown) => Err(thrown),
    }
}

/// [`adopted`] over what may be nothing.
#[cfg_attr(debug_assertions, track_caller)]
pub(crate) fn adopted_some<T: View>(
    req: &Request,
    made: Result<Option<T>, Thrown>,
) -> Result<Option<Owner<T>>, Thrown> {
    match made {
        Ok(Some(view)) => Ok(Some(Owner::adopt(req, view))),
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

impl<T: View> std::ops::Deref for Owner<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.view
    }
}

/// A debug build's sensor: an owner dropped unconsumed releases nothing
/// and is recorded on its heap with the line that made it.
#[cfg(debug_assertions)]
impl<T: View> Drop for Owner<T> {
    fn drop(&mut self) {
        if let Some(made) = self.made.take() {
            made.sensor.unconsumed(made.site);
        }
    }
}

/// An owner returned from a native is moved to the engine: the caller's
/// temporary holds it, and nothing more is taken. The value is the one
/// the count stands on, and not `T::into_return`'s, which is a view's
/// return.
impl<T: View + IntoReturn> IntoReturn for Owner<T> {
    const UNDECLARED: bool = T::UNDECLARED;
    const MASK: u32 = T::MASK;
    fn declared() -> Option<Type> {
        T::declared()
    }
    fn into_return(self, _: &mut Request) -> Result<Owner<Value>, Thrown> {
        Ok(self.into_value())
    }
}
