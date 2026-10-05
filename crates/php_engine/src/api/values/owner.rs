//! One owner of a value as a type (design §5.1, #1980): [`Owner`]
//! beside the views it owns.

use php_unit::decl::Type;

use crate::api::abi::IntoReturn;
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Key, PhpArray, PhpObject, PhpRef, PhpString, Resource, Value};

/// A view an [`Owner`] can own: [`Value`], the five refcounted kinds and
/// a [`Key`]. Sealed: the face lists them, and no extension adds one.
pub trait View: sealed::Counted + Clone {}

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
view!(Resource, Value::resource);
view!(Key, |k: Key| match k {
    Key::Int(n) => Value::int(n),
    Key::Str(s) => Value::string(s),
});

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
    /// `zend_string_copy`, and before it returns an argument or an
    /// element of one, `RETURN_COPY`.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn take(req: &Request, view: T) -> Owner<T> {
        req.addref(&view.counted());
        Owner::adopt(req, view)
    }

    /// The owner `view` already is: the count a value is made with,
    /// which only the engine hands out.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn adopt(req: &Request, view: T) -> Owner<T> {
        #[cfg(not(debug_assertions))]
        let _ = req;
        Owner {
            view,
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
        req.release(self.view.counted());
    }

    /// The view, the owner handed on with it: what a sink keeps, and what
    /// the engine is handed as a native's return.
    // Taking the owner is the point: the sink holds it from here.
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn hand_on(mut self) -> T {
        self.consume();
        self.view.clone()
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
/// temporary holds it, and nothing more is taken. The value is the
/// view's own and not `T::into_return`'s: that is a view's return, which
/// hands on the owner the view was made with until the constructors
/// return owners, and then takes an owner of its own (#2047).
impl<T: View + IntoReturn> IntoReturn for Owner<T> {
    const UNDECLARED: bool = T::UNDECLARED;
    const MASK: u32 = T::MASK;
    fn declared() -> Option<Type> {
        T::declared()
    }
    fn into_return(self, _: &mut Request) -> Result<Value, Thrown> {
        Ok(self.hand_on().counted())
    }
}
