//! One more owner of a value for the span of a call that may reach PHP
//! code: php's `GC_ADDREF` before a diagnostic, an accessor or a
//! conversion that can reach a user handler, and the `GC_DELREF`,
//! `OBJ_RELEASE` or `zend_string_release` after it. A handler may give
//! up every other owner of the array a write is landing in, the object
//! an accessor is served on or the string a conversion is reading;
//! under a [`Hold`] what it names stands until the hold is given back,
//! and what is [`Left`] then says whether the handler freed it or took
//! an owner of its own, which is the test php's handler makes after the
//! call.
//!
//! The owner is a type and not a pair of calls: [`Request::take_hold`]
//! is the one place a hold is taken, and [`Request::let_go`] and
//! [`Request::release_hold`] the two ways it is given back, php's
//! `GC_DELREF` and its `OBJ_RELEASE`. A `Hold` is neither `Copy` nor
//! `Clone`, so one taken is one given back.

use php_heap::{Heap, ZvalRef};

use super::{Hooks, Request};
use crate::api::values::Value;

/// One owner of a value, held across a call that may reach PHP code.
#[must_use = "a hold is an owner of the value, given back through Request::let_go"]
#[derive(Debug)]
pub(crate) struct Hold(Value);

impl Hold {
    /// What the hold names; the copy owns nothing of its own.
    pub(crate) fn value(&self) -> Value {
        self.0
    }
}

/// What is left of a held value once the hold is given back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Left {
    /// Nothing was held: a scalar, an interned string or an immutable
    /// array, which no handler frees (php's `IS_ARRAY_IMMUTABLE` test
    /// before its `GC_ADDREF`).
    Uncounted,
    /// The owners besides the hold as they stand after it; none is the
    /// value freed as the hold was given back.
    Owners(u32),
}

impl Left {
    /// The hold was the last owner, and the value is gone:
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

/// Where the last `FETCH_DIM_W` laid the `IS_INDIRECT` of its `VAR`,
/// and the array or object whose table the entry is in. What it names
/// is owned by whatever held the container when the fetch walked it,
/// and no PHP code may be reached between the fetch and the one read
/// of it: [`Request::hold_walk`] turns it into an owner or into
/// nothing, and the `ASSIGN_DIM` it is read by stands next after the
/// fetch that wrote it, as php's delayed fetch compiles them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Walk {
    place: ZvalRef,
    table: Value,
}

impl Request {
    /// What the `FETCH_DIM_W` just served laid down: the entry its
    /// `VAR` points at and the array or object that holds the entry, or
    /// nothing where the `VAR` holds no `IS_INDIRECT`. Every fetch
    /// notes one or the other, so what is noted is its own.
    pub(crate) fn note_walk(&mut self, walked: Option<(ZvalRef, Value)>) {
        self.walk = walked.map(|(place, table)| Walk { place, table });
    }

    /// An owner of the table the `FETCH_DIM_W` before this opline walked
    /// to `place`, taken before PHP code can be reached: nothing where the
    /// fetch noted another place or none.
    pub(crate) fn hold_walk(&mut self, place: ZvalRef) -> Option<Hold> {
        let walk = self.walk.take()?;
        (walk.place == place).then(|| self.take_hold(walk.table))
    }

    /// One more owner of `value` until it is given back:
    /// `GC_ADDREF`, or `GC_TRY_ADDREF` over what may be immutable.
    pub(crate) fn take_hold(&self, value: Value) -> Hold {
        self.addref(&value);
        Hold(value)
    }

    /// The hold given back as `GC_DELREF` gives it, and what is left of
    /// the value: at the last owner the value is destroyed,
    /// `zend_array_destroy`, `zend_string_efree` or
    /// `zend_objects_store_del` with its destructor served, and where
    /// owners are left nothing else happens, no possible root of the
    /// cycle collector (design §5.12). What php's handlers do after a
    /// diagnostic.
    pub(crate) fn let_go(&mut self, hold: Hold) -> Left {
        self.give_back(hold, false)
    }

    /// The hold given back as `OBJ_RELEASE` and `zval_ptr_dtor` give an
    /// owner back: [`Request::let_go`], and where owners are left the
    /// value a possible root (`gc_check_possible_root`). What php's
    /// standard object handlers do after an accessor or an
    /// `ArrayAccess` method.
    pub(crate) fn release_hold(&mut self, hold: Hold) -> Left {
        self.give_back(hold, true)
    }

    // Taking the hold is the point: it is the owner given up here, and
    // one given back cannot be given back again.
    #[allow(clippy::needless_pass_by_value)]
    fn give_back(&mut self, hold: Hold, root: bool) -> Left {
        let Hold(value) = hold;
        let zval = value.to_zval();
        if !zval.is_refcounted() {
            return Left::Uncounted;
        }
        let owners = value.refcount().saturating_sub(1);
        if root {
            Heap::release(&mut Hooks(self), zval);
        } else {
            Heap::release_nogc(&mut Hooks(self), zval);
        }
        Left::Owners(owners)
    }
}
